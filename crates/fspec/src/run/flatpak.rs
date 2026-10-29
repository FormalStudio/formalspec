//! `fspec run flatpak:<id>`: a Flathub app installed into fspec's own flatpak installation and
//! run from there. It is flatpak's per-user installation moved by `FLATPAK_USER_DIR` (flatpak(1),
//! ENVIRONMENT) to `~/.cache/fspec/dev/flatpak/installation`, so the user's `flatpak list`, its
//! exported desktop files and the spec never see it. Runtimes the system installation already has
//! are used from there, as flatpak does for any per-user install; the rest are downloaded beside
//! the app. Each run touches `used/<id>`, and `fspec gc --older-than` uninstalls the apps not run
//! within the age.

use std::io::{BufRead, BufReader, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use fspec_core::Paths;
use fspec_core::activity::{self, Kind, Operation, Phase, Unit};

const FLATHUB: &str = "https://dl.flathub.org/repo/flathub.flatpakrepo";

/// `org.mozilla.firefox`: three or more dot-separated parts, as flatpak names apps.
pub fn is_app_id(id: &str) -> bool {
    let parts: Vec<&str> = id.split('.').collect();
    parts.len() >= 3
        && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || "_-".contains(c)))
}

fn root(home: &Path) -> PathBuf {
    fspec_dev::cache_dir(home).join("flatpak")
}

/// `flatpak --user` against fspec's installation.
fn flatpak(root: &Path) -> Command {
    let mut c = Command::new("flatpak");
    c.env("FLATPAK_USER_DIR", root.join("installation")).arg("--user");
    c
}

pub fn run(
    paths: &Paths,
    id: &str,
    bin: Option<&str>,
    update: bool,
    detach: bool,
    args: &[String],
) -> Result<Option<Child>> {
    let root = root(&paths.home);
    let name = id.rsplit('.').next().unwrap_or(id);
    let op = Operation::new(Kind::Run, &format!("flatpak:{id}"), format!("Running {name}"), None);
    activity::run(op, || {
        let installed = flatpak(&root)
            .args(["info", id])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|e| missing(e, id))?
            .success();
        if !installed || update {
            activity::phase(Phase::Preparing, None);
            let ok = flatpak(&root)
                .args(["remote-add", "--if-not-exists", "flathub", FLATHUB])
                .status()
                .context("adding Flathub to fspec's flatpak installation")?;
            if !ok.success() {
                bail!("adding Flathub to fspec's flatpak installation failed");
            }
            let verb: &[&str] = if installed { &["update", "-y", id] } else { &["install", "-y", "flathub", id] };
            download(&root, verb, name)?;
        }
        let used = root.join("used");
        std::fs::create_dir_all(&used).with_context(|| format!("creating {}", used.display()))?;
        // Its age is what `fspec gc` reads as the last use.
        std::fs::write(used.join(id), "")?;
        let mut c = flatpak(&root);
        c.arg("run");
        if let Some(b) = bin {
            c.arg(format!("--command={b}"));
        }
        c.arg(id).args(args);
        super::start(c, detach, id)
    })
}

fn missing(e: std::io::Error, id: &str) -> anyhow::Error {
    if e.kind() == std::io::ErrorKind::NotFound {
        anyhow::anyhow!(
            "flatpak:{id} needs flatpak, which is not installed; add `pkg(\"flatpak\", source = distro())` \
             to the spec and run `fspec apply`"
        )
    } else {
        anyhow::Error::new(e).context("starting flatpak")
    }
}

/// Run flatpak's install or update, its progress on the operation stream and, in a terminal, on
/// one line of stderr.
fn download(root: &Path, verb: &[&str], name: &str) -> Result<()> {
    // With stdout not a terminal, flatpak prints a line per progress step and takes `-y` for
    // every question, the app's permissions included.
    let mut child = flatpak(root)
        .args(verb)
        // The progress lines are parsed, so untranslated.
        .env("LC_ALL", "C.UTF-8")
        .env_remove("LANGUAGE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting flatpak")?;
    let mut stderr = child.stderr.take().expect("piped");
    let errors = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    let tty = std::io::stderr().is_terminal();
    let mut last = None;
    for line in BufReader::new(child.stdout.take().expect("piped")).lines() {
        let Some(pct) = percent(&line?) else { continue };
        if last == Some(pct) {
            continue;
        }
        last = Some(pct);
        activity::progress(pct, 100, Unit::Percent, Some(name.to_string()), vec![]);
        if tty {
            eprint!("\r\x1b[K{name}: downloading {pct}%");
            let _ = std::io::stderr().flush();
        }
    }
    if tty && last.is_some() {
        eprintln!();
    }
    let status = child.wait().context("waiting for flatpak")?;
    let errors = errors.join().unwrap_or_default();
    if !status.success() {
        bail!("flatpak {} failed: {}", verb.join(" "), errors.trim());
    }
    Ok(())
}

/// The whole download's share done off one of flatpak's progress lines,
/// `Installing 2/3… ████     43%  1.2 MB/s`, or `Installing… ██ 43%` when there is one ref.
fn percent(line: &str) -> Option<u64> {
    let rest = line.strip_prefix("Installing").or_else(|| line.strip_prefix("Updating"))?;
    let (k, n): (u64, u64) = match rest.strip_prefix(' ') {
        Some(counted) => {
            let (k, rest) = counted.split_once('/')?;
            let n: String = rest.chars().take_while(char::is_ascii_digit).collect();
            (k.parse().ok()?, n.parse().ok()?)
        }
        None if rest.starts_with('…') || rest.starts_with("...") => (1, 1),
        None => return None,
    };
    let pct = line.split_whitespace().rev().find_map(|w| w.strip_suffix('%')?.parse::<u64>().ok()).unwrap_or(0);
    (k >= 1 && n >= k).then(|| ((k - 1) * 100 + pct.min(100)) / n)
}

/// Uninstall the apps not run since `cutoff`, then the runtimes nothing installed there needs; the
/// whole installation goes once no app is left. Also forgets the bare names not used since.
pub fn gc(home: &Path, cutoff: SystemTime) -> Result<Vec<PathBuf>> {
    sweep(&fspec_dev::cache_dir(home), cutoff)
}

fn sweep(cache: &Path, cutoff: SystemTime) -> Result<Vec<PathBuf>> {
    let mut removed = vec![];
    let names = cache.join("names");
    for e in std::fs::read_dir(&names).into_iter().flatten().flatten() {
        if stale(&e.path(), cutoff) {
            std::fs::remove_file(e.path())?;
            removed.push(e.path());
        }
    }
    let root = cache.join("flatpak");
    let mut left = 0;
    let mut gone = vec![];
    for e in std::fs::read_dir(root.join("used")).into_iter().flatten().flatten() {
        if stale(&e.path(), cutoff) {
            gone.push(e);
        } else {
            left += 1;
        }
    }
    if left == 0 && root.exists() {
        fspec_core::journal::remove_any(&root)?;
        removed.push(root);
        return Ok(removed);
    }
    for e in &gone {
        let id = e.file_name().to_string_lossy().into_owned();
        let present = flatpak(&root).args(["info", &id]).output().is_ok_and(|o| o.status.success());
        if !present {
            std::fs::remove_file(e.path())?;
            continue;
        }
        let out = flatpak(&root).args(["uninstall", "-y", "--noninteractive", &id]).output()?;
        if !out.status.success() {
            bail!("flatpak uninstall {id} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        std::fs::remove_file(e.path())?;
        removed.push(root.join("installation/app").join(&id));
    }
    if !gone.is_empty() {
        let out = flatpak(&root).args(["uninstall", "-y", "--noninteractive", "--unused"]).output()?;
        if !out.status.success() {
            bail!("flatpak uninstall --unused failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
    }
    Ok(removed)
}

fn stale(path: &Path, cutoff: SystemTime) -> bool {
    std::fs::metadata(path).and_then(|m| m.modified()).map_or(true, |t| t <= cutoff)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_ids_are_reverse_dns() {
        assert!(is_app_id("org.mozilla.firefox"));
        assert!(is_app_id("com.github.tchx84.Flatseal"));
        assert!(!is_app_id("firefox"));
        assert!(!is_app_id("org..firefox"));
        assert!(!is_app_id("org.mozilla.firefox/x86_64/stable"));
    }

    #[test]
    fn progress_lines_give_the_whole_downloads_share() {
        assert_eq!(percent("Installing 1/2… ▊                      4%"), Some(2));
        assert_eq!(percent("Installing 2/2… ████████     50%  1.2 MB/s"), Some(75));
        assert_eq!(percent("Installing 2/2…"), Some(50));
        assert_eq!(percent("Updating 1/1… ████ 100%  2 MB/s"), Some(100));
        assert_eq!(percent("Installing… ███     30%  4.1 MB/s"), Some(30));
        assert_eq!(percent("Updating…"), Some(0));
        assert_eq!(percent("Installation complete."), None);
        assert_eq!(percent("Installation complete."), None);
        assert_eq!(percent("Looking for matches…"), None);
    }

    #[test]
    fn gc_with_no_app_used_since_drops_the_installation_and_stale_names() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path();
        let root = cache.join("flatpak");
        std::fs::create_dir_all(root.join("used")).unwrap();
        std::fs::create_dir_all(root.join("installation/repo")).unwrap();
        std::fs::write(root.join("used/org.mozilla.firefox"), "").unwrap();
        std::fs::create_dir_all(cache.join("names")).unwrap();
        std::fs::write(cache.join("names/abc"), "flatpak:org.mozilla.firefox").unwrap();
        let removed = sweep(cache, SystemTime::now() + std::time::Duration::from_secs(60)).unwrap();
        assert!(!root.exists() && !cache.join("names/abc").exists(), "{removed:?}");
        assert_eq!(removed.len(), 2);
    }
}
