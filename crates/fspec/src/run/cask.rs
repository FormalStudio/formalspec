//! `fspec run brew:<cask>` on macOS: a Homebrew cask's app downloaded, checked against the cask's
//! sha256 and unpacked under `~/.cache/fspec/dev/casks/<token>/<version>/`, then started from
//! there. Nothing goes in /Applications or Homebrew's records, and no privilege is needed. A cask
//! whose app comes in a `.pkg` installer needs root and is refused, as are formulae.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::SystemTime;

use anyhow::{Context, Result, anyhow, bail};
use fspec_core::Paths;
use fspec_core::activity::{self, Kind, Operation, Phase, Unit};
use fspec_core::fetch::{Fetch, LiveFetch};
use serde_json::Value;

/// What running a cask needs from its API entry.
#[derive(Debug, PartialEq, Eq)]
struct Cask {
    version: String,
    url: String,
    /// `None` for a cask declared `sha256 :no_check`.
    sha256: Option<String>,
    /// The app's path inside what the url downloads, `Firefox.app`.
    app: String,
}

fn parse(token: &str, doc: &Value, arm: bool) -> Result<Cask> {
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    // The top level is the newest macOS on Apple silicon; Intel builds are named by codename alone.
    let intel = (!arm)
        .then(|| doc.get("variations")?.as_object()?.iter().find(|(k, _)| !k.contains('_')).map(|(_, v)| v.clone()))
        .flatten();
    let pick = |k: &str| intel.as_ref().and_then(|v| s(v, k)).or_else(|| s(doc, k));
    let url = pick("url").ok_or_else(|| anyhow!("cask {token} has no url"))?;
    let sha256 = pick("sha256").filter(|h| h.len() == 64);
    if doc.get("container").is_some_and(|c| !c.is_null()) {
        bail!("cask {token} unpacks a nested container, which fspec run does not open");
    }
    let artifacts = doc.get("artifacts").and_then(Value::as_array).cloned().unwrap_or_default();
    let app = artifacts.iter().find_map(|a| a.get("app")?.as_array()?.first()?.as_str().map(str::to_string));
    let Some(app) = app else {
        let kinds: Vec<String> = artifacts
            .iter()
            .filter_map(|a| a.as_object()?.keys().next().cloned())
            .filter(|k| k != "uninstall")
            .collect();
        bail!(
            "cask {token} has no app to start (it installs {}); to install it, add \
             `pkg({token:?}, source = brew(cask = True))` to the spec and run `fspec apply`",
            kinds.join(", ")
        );
    };
    if app.split('/').any(|p| p == ".." || p.is_empty()) {
        bail!("cask {token} names its app as {app}");
    }
    let version = s(doc, "version").unwrap_or_else(|| "latest".into()).replace(['/', ','], "-");
    Ok(Cask { version, url, sha256, app })
}

fn root(home: &Path, token: &str) -> PathBuf {
    fspec_dev::cache_dir(home).join("casks").join(token)
}

pub fn run(
    paths: &Paths,
    token: &str,
    bin: Option<&str>,
    update: bool,
    detach: bool,
    args: &[String],
) -> Result<Option<Child>> {
    let root = root(&paths.home, token);
    // `<version>/<app>` of the copy last started; its age is what `fspec gc` reads as the last use.
    let current = root.join("current");
    let op = Operation::new(Kind::Run, &format!("brew:{token}"), format!("Running {token}"), None);
    activity::run(op, || {
        let kept = std::fs::read_to_string(&current).ok().map(|r| root.join(r.trim())).filter(|a| a.is_dir());
        let app = match kept {
            Some(app) if !update => {
                std::fs::write(&current, std::fs::read_to_string(&current)?)?;
                app
            }
            _ => {
                activity::phase(Phase::Resolving, None);
                let api = LiveFetch::with_timeout(Some(std::time::Duration::from_secs(15)));
                let doc = match api.get_json(&format!("https://formulae.brew.sh/api/cask/{token}.json")) {
                    Err(e) if fspec_core::fetch::is_not_found(&e) => bail!(
                        "`{token}` is not a Homebrew cask, and a formula cannot run without being installed; \
                         to install it, add `pkg({token:?}, source = brew())` to the spec and run `fspec apply`"
                    ),
                    r => r?,
                };
                let cask = parse(token, &doc, cfg!(target_arch = "aarch64"))?;
                let app = fetch(&root, token, &cask)?;
                std::fs::write(&current, format!("{}/{}", cask.version, cask.app))?;
                app
            }
        };
        let c = if detach {
            let mut c = Command::new("open");
            c.arg("-a").arg(&app);
            if let Some(b) = bin {
                bail!("--bin {b} needs a run in the foreground; `open` starts the app's own executable");
            }
            c.arg("--args").args(args);
            c
        } else {
            let mut c = Command::new(executable(&app, bin)?);
            c.args(args);
            c
        };
        super::start(c, detach, token)
    })
}

/// The app's own executable, or `bin` beside it in `Contents/MacOS`.
fn executable(app: &Path, bin: Option<&str>) -> Result<PathBuf> {
    let name = match bin {
        Some(b) => b.to_string(),
        None => {
            let out = Command::new("plutil")
                .args(["-extract", "CFBundleExecutable", "raw", "-o", "-"])
                .arg(app.join("Contents/Info.plist"))
                .output()
                .context("starting plutil")?;
            if !out.status.success() {
                bail!("{} names no CFBundleExecutable", app.display());
            }
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }
    };
    let exe = app.join("Contents/MacOS").join(&name);
    if !exe.is_file() {
        bail!("{} has no executable {name}", app.display());
    }
    Ok(exe)
}

/// Download, check and unpack `cask` into `root/<version>`, older versions removed after.
fn fetch(root: &Path, token: &str, cask: &Cask) -> Result<PathBuf> {
    std::fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
    activity::phase(Phase::Preparing, None);
    let part = root.join("download.part");
    let tty = std::io::stderr().is_terminal();
    let mut last = None;
    let sha = LiveFetch::new().download(&cask.url, &part, &mut |done, total| {
        let Some(total) = total.filter(|t| *t > 0) else { return };
        let pct = done * 100 / total;
        if last != Some(pct) {
            last = Some(pct);
            activity::progress(pct, 100, Unit::Percent, Some(token.to_string()), vec![]);
            if tty {
                eprint!("\r\x1b[K{token}: downloading {pct}%");
                let _ = std::io::stderr().flush();
            }
        }
    })?;
    if tty && last.is_some() {
        eprintln!();
    }
    if let Some(want) = &cask.sha256
        && !want.eq_ignore_ascii_case(&sha)
    {
        let _ = std::fs::remove_file(&part);
        bail!("{} has sha256 {sha}, not the cask's {want}", cask.url);
    }
    let stage = root.join(format!("{}.part", cask.version));
    let _ = fspec_core::journal::remove_any(&stage);
    std::fs::create_dir_all(&stage)?;
    let unpacked = unpack(&part, &stage, &cask.app);
    let _ = std::fs::remove_file(&part);
    if let Err(e) = unpacked {
        let _ = fspec_core::journal::remove_any(&stage);
        return Err(e.context(format!("unpacking cask {token}")));
    }
    if !stage.join(&cask.app).is_dir() {
        bail!("cask {token}'s download holds no {}", cask.app);
    }
    let dest = root.join(&cask.version);
    let _ = fspec_core::journal::remove_any(&dest);
    std::fs::rename(&stage, &dest)?;
    for e in std::fs::read_dir(root)?.flatten() {
        if e.path().is_dir() && e.file_name() != cask.version.as_str() {
            fspec_core::journal::remove_any(&e.path())?;
        }
    }
    Ok(dest.join(&cask.app))
}

#[derive(Debug, PartialEq, Eq)]
enum Container {
    Zip,
    Tar,
    Dmg,
    Pkg,
}

/// What a download is, off its bytes: casks' urls often carry no extension.
fn container(head: &[u8], tail: &[u8]) -> Option<Container> {
    // A UDIF image's trailer is its last 512 bytes, and its data may open like a bzip2 stream.
    if tail.starts_with(b"koly") {
        Some(Container::Dmg)
    } else if head.starts_with(b"PK\x03\x04") {
        Some(Container::Zip)
    } else if head.starts_with(b"xar!") {
        Some(Container::Pkg)
    } else if head.starts_with(&[0x1f, 0x8b])
        || head.starts_with(&[0xfd, b'7', b'z', b'X', b'Z'])
        || head.starts_with(b"BZh")
    {
        Some(Container::Tar)
    } else {
        None
    }
}

fn unpack(file: &Path, dest: &Path, app: &str) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(file)?;
    let mut head = [0u8; 8];
    let n = f.read(&mut head)?;
    let mut tail = [0u8; 512];
    let len = f.metadata()?.len();
    if len >= 512 {
        f.seek(SeekFrom::Start(len - 512))?;
        f.read_exact(&mut tail)?;
    }
    let ok = |c: &mut Command, what: &str| -> Result<()> {
        let out = c.stdin(Stdio::null()).output().with_context(|| format!("starting {what}"))?;
        if !out.status.success() {
            bail!("{what} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(())
    };
    match container(&head[..n], &tail) {
        Some(Container::Zip) => ok(Command::new("ditto").args(["-x", "-k"]).arg(file).arg(dest), "ditto"),
        Some(Container::Tar) => ok(Command::new("tar").arg("-xf").arg(file).arg("-C").arg(dest), "tar"),
        Some(Container::Dmg) => {
            let mnt = dest.with_extension("mnt");
            std::fs::create_dir_all(&mnt)?;
            ok(
                Command::new("hdiutil")
                    .args(["attach", "-nobrowse", "-readonly", "-noautoopen", "-noverify", "-mountpoint"])
                    .arg(&mnt)
                    .arg(file),
                "hdiutil attach",
            )?;
            let copied = ok(Command::new("ditto").arg(mnt.join(app)).arg(dest.join(app)), "ditto");
            let detached = ok(Command::new("hdiutil").args(["detach", "-force"]).arg(&mnt), "hdiutil detach");
            let _ = std::fs::remove_dir(&mnt);
            copied.and(detached)
        }
        Some(Container::Pkg) => bail!("it installs with a .pkg, which needs root"),
        None => bail!("its download is neither a zip, a tarball nor a disk image"),
    }
}

/// Remove the casks not started since `cutoff`.
pub fn gc(home: &Path, cutoff: SystemTime) -> Result<Vec<PathBuf>> {
    sweep(&fspec_dev::cache_dir(home).join("casks"), cutoff)
}

fn sweep(dir: &Path, cutoff: SystemTime) -> Result<Vec<PathBuf>> {
    let mut removed = vec![];
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let used = std::fs::metadata(e.path().join("current")).and_then(|m| m.modified());
        if used.map_or(true, |t| t <= cutoff) {
            fspec_core::journal::remove_any(&e.path())?;
            removed.push(e.path());
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_cask_gives_its_url_hash_and_app_for_this_arch() {
        let doc = json!({
            "version": "1.144.0",
            "url": "https://example.com/darwin-arm64",
            "sha256": "a".repeat(64),
            "variations": {
                "sequoia": {"url": "https://example.com/darwin", "sha256": "b".repeat(64)},
                "arm64_big_sur": {"url": "https://example.com/old-arm"},
            },
            "artifacts": [{"uninstall": []}, {"app": ["Visual Studio Code.app"], "target": "/Applications/x"}],
        });
        let arm = parse("code", &doc, true).unwrap();
        assert_eq!(arm.url, "https://example.com/darwin-arm64");
        assert_eq!(arm.app, "Visual Studio Code.app");
        assert_eq!(arm.sha256, Some("a".repeat(64)));
        let intel = parse("code", &doc, false).unwrap();
        assert_eq!((intel.url.as_str(), intel.sha256), ("https://example.com/darwin", Some("b".repeat(64))));
        let no_check = json!({"url": "u", "sha256": "no_check", "artifacts": [{"app": ["A.app"]}]});
        assert_eq!(parse("a", &no_check, true).unwrap().sha256, None);
    }

    #[test]
    fn a_cask_with_no_app_says_how_to_install_it() {
        let doc = json!({"url": "u", "artifacts": [{"pkg": ["X.pkg"]}, {"uninstall": []}]});
        assert_eq!(
            parse("x", &doc, true).unwrap_err().to_string(),
            "cask x has no app to start (it installs pkg); to install it, add \
             `pkg(\"x\", source = brew(cask = True))` to the spec and run `fspec apply`"
        );
        assert!(parse("x", &json!({"url": "u", "artifacts": [{"app": ["../A.app"]}]}), true).is_err());
    }

    #[test]
    fn downloads_are_told_apart_by_their_bytes() {
        let none = [0u8; 512];
        let mut koly = [0u8; 512];
        koly[..4].copy_from_slice(b"koly");
        assert_eq!(container(b"PK\x03\x04", &none), Some(Container::Zip));
        assert_eq!(container(b"xar!", &none), Some(Container::Pkg));
        assert_eq!(container(&[0x1f, 0x8b, 8], &none), Some(Container::Tar));
        assert_eq!(container(b"\0\0\0\0", &koly), Some(Container::Dmg));
        assert_eq!(container(b"BZh91AY", &koly), Some(Container::Dmg));
        assert_eq!(container(b"\0\0\0\0", &none), None);
    }

    #[test]
    fn gc_drops_casks_not_started_since() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("firefox/159.0/Firefox.app")).unwrap();
        std::fs::write(tmp.path().join("firefox/current"), "159.0/Firefox.app").unwrap();
        assert!(sweep(tmp.path(), SystemTime::now() - std::time::Duration::from_secs(60)).unwrap().is_empty());
        let removed = sweep(tmp.path(), SystemTime::now() + std::time::Duration::from_secs(60)).unwrap();
        assert_eq!(removed, [tmp.path().join("firefox")]);
    }
}
