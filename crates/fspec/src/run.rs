//! `fspec run`: one package run without adding it to the spec. A store package is a one-package
//! `dev.star` in the cache, activated as `fspec dev` activates a project, so its store paths live
//! and go the way a project's do (`fspec gc`); a Flathub app goes into fspec's own flatpak
//! installation ([`flatpak`]). docs/shell-install.md has the design.

mod cask;
pub mod flatpak;

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use anyhow::{Context, Result, bail};
use fspec_core::activity::{self, Kind, Operation, Phase};
use fspec_core::fetch::LiveFetch;
use fspec_core::runner::LiveRunner;
use fspec_core::{Lock, Paths};
use fspec_dev::Project;
use fspec_sources::catalog;

/// Drop what `fspec run` fetched outside the user store and has not run since `cutoff`: Flathub
/// apps, casks and remembered bare names.
pub fn gc(home: &Path, cutoff: std::time::SystemTime) -> Result<Vec<PathBuf>> {
    let mut removed = flatpak::gc(home, cutoff)?;
    removed.extend(cask::gc(home, cutoff)?);
    Ok(removed)
}

/// What `fspec run` was asked for: the package's name, and its `pkg(...)` in a `dev.star`.
#[derive(Debug, PartialEq, Eq)]
pub struct Declared {
    pub name: String,
    pub pkg: String,
}

/// What a `fspec run` argument names.
#[derive(Debug, PartialEq, Eq)]
pub enum Target {
    Store(Declared),
    /// A Flathub app id.
    Flatpak(String),
    /// A Homebrew cask's token, on macOS.
    Cask(String),
}

/// `ripgrep`, `aqua:ripgrep`, `store:ripgrep`, `github:sharkdp/bat`, `npm:@scope/tool`,
/// `cargo:xh`, `flatpak:org.mozilla.firefox`. A distro or Homebrew package has to be installed.
pub fn declare(spec: &str) -> Result<Target> {
    let (source, arg) = spec.split_once(':').unwrap_or(("aqua", spec));
    let safe = |c: char| c.is_ascii_alphanumeric() || "._-/@+".contains(c);
    if arg.is_empty() || !arg.chars().all(safe) {
        bail!("`{spec}` is not a package name");
    }
    let last = arg.rsplit('/').next().unwrap_or(arg).to_string();
    let pkg = match source {
        "aqua" | "store" => format!("pkg({arg:?})"),
        "github" if arg.matches('/').count() == 1 => format!("pkg({last:?}, source = github({arg:?}))"),
        "npm" => format!("pkg({last:?}, source = npm({arg:?}))"),
        "cargo" => format!("pkg({last:?}, source = cargo({arg:?}))"),
        "flatpak" if flatpak::is_app_id(arg) => return Ok(Target::Flatpak(arg.to_string())),
        "flatpak" => bail!("`{arg}` is not a Flathub app id like org.mozilla.firefox"),
        "github" => bail!("github:{arg} must be owner/repo"),
        "brew" | "cask" if cfg!(target_os = "macos") && !arg.contains('/') => {
            return Ok(Target::Cask(arg.to_string()));
        }
        "zypper" | "pacman" | "dnf" | "apt" | "brew" => bail!(
            "{source} packages cannot run without being installed; `fspec app install {source}:{arg}` \
             installs it through the spec"
        ),
        "xbps" => bail!(
            "xbps packages cannot run without being installed; to install it, add \
             `pkg({arg:?}, source = xbps())` to the spec and run `fspec apply`"
        ),
        other => bail!("fspec run takes aqua, github, npm, cargo, flatpak and (on macOS) brew packages, not {other}"),
    };
    let name = if matches!(source, "aqua" | "store") { arg.to_string() } else { last };
    Ok(Target::Store(Declared { name, pkg }))
}

/// Where run projects live: beside the dev link farms, whose gc they share.
pub fn runs_dir(home: &Path) -> PathBuf {
    fspec_dev::cache_dir(home).join("once")
}

pub fn run(paths: &Paths, spec: &str, bin: Option<&str>, update: bool, detach: bool, args: &[String]) -> Result<()> {
    let target = declare(spec)?;
    let spec = if spec.contains(':') { spec.to_string() } else { meant(paths, spec, &target, update)? };
    let child = match declare(&spec)? {
        Target::Store(d) => run_store(paths, &spec, &d, bin, update, detach, args)?,
        Target::Flatpak(id) => flatpak::run(paths, &id, bin, update, detach, args)?,
        Target::Cask(token) => cask::run(paths, &token, bin, update, detach, args)?,
    };
    let Some(mut child) = child else { return Ok(()) };
    // The terminal's Ctrl-C is the command's to handle; fspec only passes its exit status on.
    // SAFETY: setting a signal disposition to SIG_IGN and back.
    let old = unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) };
    let status = child.wait();
    // SAFETY: as above.
    unsafe { libc::signal(libc::SIGINT, old) };
    let code = status.with_context(|| format!("waiting for {spec}"))?.code().unwrap_or(1);
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

/// What a bare `name` runs: itself when aqua has it, else the one package of that name the
/// catalogs clearly mean (`firefox` is `flatpak:org.mozilla.firefox`), remembered under
/// `~/.cache/fspec/dev/names/` until `--update` or `fspec gc`. With no clear one it fails listing
/// the candidates.
fn meant(paths: &Paths, name: &str, target: &Target, update: bool) -> Result<String> {
    let Target::Store(d) = target else { return Ok(name.to_string()) };
    let memo = names_dir(&paths.home).join(fspec_dev::short_hash(name));
    if !update {
        if let Ok(id) = std::fs::read_to_string(&memo) {
            touch(&memo);
            eprintln!("{name}: running {}", id.trim());
            return Ok(id.trim().to_string());
        }
        // Resolved before: aqua has it.
        if (Project { dir: store_dir(paths, d).1 }).lock_path().is_file() {
            return Ok(name.to_string());
        }
    }
    let fetch = LiveFetch::with_timeout(Some(std::time::Duration::from_secs(15)));
    let cx = fspec_sources::Cx::new(&fetch, &LiveRunner).with_catalog(crate::search::catalog(paths, true));
    if catalog::aqua_has(name, &cx)? {
        return Ok(name.to_string());
    }
    let found = catalog::search(name, &cx);
    let Some(hit) = catalog::pick(name, &found) else {
        let similar = catalog::suggest_from(&found).map(|s| format!("; {s}")).unwrap_or_default();
        bail!("`{name}` is not in the aqua registry{similar}");
    };
    let id = hit.run_arg().expect("pick only takes what runs");
    eprintln!("{name}: running {id}");
    std::fs::create_dir_all(names_dir(&paths.home))?;
    std::fs::write(&memo, &id)?;
    Ok(id)
}

/// Bare names and the package each was taken to mean.
fn names_dir(home: &Path) -> PathBuf {
    fspec_dev::cache_dir(home).join("names")
}

fn touch(path: &Path) {
    let _ = std::fs::File::options().write(true).open(path).and_then(|f| f.set_modified(std::time::SystemTime::now()));
}

/// A store package's `dev.star` and the directory it is run from.
fn store_dir(paths: &Paths, d: &Declared) -> (String, PathBuf) {
    let (platform, _) = fspec_core::facts::Facts::probe_local();
    let source = format!("dev(packages = [{}], platforms = [{platform:?}])\n", d.pkg);
    let dir = runs_dir(&paths.home).join(fspec_dev::short_hash(&source));
    (source, dir)
}

/// Start `fspec run`'s child, `detach`ed into a session of its own or not.
fn start(mut c: Command, detach: bool, what: &str) -> Result<Option<Child>> {
    if detach {
        c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        // SAFETY: setsid is async-signal-safe and the child calls nothing else before exec.
        unsafe {
            c.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    // Spawned inside the operation, so a command that cannot start fails it.
    let child = c.spawn().with_context(|| format!("starting {what}"))?;
    Ok((!detach).then_some(child))
}

fn run_store(
    paths: &Paths,
    spec: &str,
    d: &Declared,
    bin: Option<&str>,
    update: bool,
    detach: bool,
    args: &[String],
) -> Result<Option<Child>> {
    let (source, dir) = store_dir(paths, d);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    // Written on every run: its age is what `fspec gc` reads as the last use.
    std::fs::write(dir.join(fspec_dev::FILE), &source)?;
    let op = Operation::new(Kind::Run, spec, format!("Running {}", d.name), None);
    activity::run(op, || {
        let project = Project { dir: dir.clone() };
        let fetch = LiveFetch::new();
        if update || !project.lock_path().is_file() {
            activity::phase(Phase::Resolving, None);
            let cx = fspec_sources::Cx::new(&fetch, &LiveRunner)
                .with_spec_dir(&dir)
                .with_catalog(crate::search::catalog(paths, true));
            let u = fspec_dev::update(&project, &[], &cx, paths)?;
            if !u.errors.is_empty() {
                bail!("{}", u.errors.join("\n"));
            }
            if u.lock != Lock::load(&project.lock_path())? {
                u.lock.save(&project.lock_path())?;
            }
        }
        activity::phase(Phase::Preparing, None);
        let m = crate::dev::machine(paths, &fetch);
        let a = fspec_dev::activate(&project, &m)?;
        let exe = pick(&a.farm.join("bin"), &d.name, bin)?;
        let mut c = Command::new(exe);
        c.args(args).env_clear().envs(&a.env);
        start(c, detach, &d.name)
    })
}

/// `bin`, else the package's only command, else the one named after it.
fn pick(dir: &Path, name: &str, bin: Option<&str>) -> Result<PathBuf> {
    let mut cmds: Vec<String> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    cmds.sort();
    let chosen = match bin {
        Some(b) if cmds.iter().any(|c| c == b) => b.to_string(),
        Some(b) => bail!("{name} has no command `{b}`; it has {}", cmds.join(", ")),
        None if cmds.len() == 1 => cmds[0].clone(),
        None if cmds.iter().any(|c| c == name) => name.to_string(),
        None if cmds.is_empty() => bail!("{name} has no commands to run"),
        None => bail!("{name} has several commands ({}); pick one with --bin", cmds.join(", ")),
    };
    Ok(dir.join(chosen))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_an_aqua_package_and_a_prefix_picks_the_source() {
        let d = |s: &str| match declare(s).unwrap() {
            Target::Store(d) => d,
            t => panic!("{t:?}"),
        };
        assert_eq!(d("ripgrep"), Declared { name: "ripgrep".into(), pkg: r#"pkg("ripgrep")"#.into() });
        assert_eq!(d("github:sharkdp/bat").pkg, r#"pkg("bat", source = github("sharkdp/bat"))"#);
        assert_eq!(
            d("npm:@scope/tool"),
            Declared { name: "tool".into(), pkg: r#"pkg("tool", source = npm("@scope/tool"))"#.into() }
        );
        assert_eq!(d("cargo:xh").pkg, r#"pkg("xh", source = cargo("xh"))"#);
    }

    #[test]
    fn a_flathub_id_runs_from_fspecs_own_installation() {
        assert_eq!(declare("flatpak:org.mozilla.firefox").unwrap(), Target::Flatpak("org.mozilla.firefox".into()));
        assert!(declare("flatpak:firefox").is_err());
        assert!(declare("flatpak:org.mozilla.firefox/x86_64/stable").is_err());
    }

    #[test]
    fn what_cannot_run_uninstalled_or_could_break_out_of_the_string_is_refused() {
        assert_eq!(
            declare("zypper:MozillaFirefox").unwrap_err().to_string(),
            "zypper packages cannot run without being installed; `fspec app install zypper:MozillaFirefox` \
             installs it through the spec"
        );
        if cfg!(target_os = "macos") {
            assert_eq!(declare("brew:firefox").unwrap(), Target::Cask("firefox".into()));
            assert_eq!(declare("cask:firefox").unwrap(), Target::Cask("firefox".into()));
        } else {
            assert!(declare("brew:htop").unwrap_err().to_string().contains("`fspec app install brew:htop`"));
        }
        assert!(declare(r#"rip"grep"#).is_err());
        assert!(declare("github:bat").is_err());
        assert!(declare("nix:hello").is_err());
    }

    #[test]
    fn the_command_is_the_only_one_or_the_one_named_after_the_package() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("rg"), "").unwrap();
        assert_eq!(pick(dir.path(), "ripgrep", None).unwrap(), dir.path().join("rg"));
        std::fs::write(dir.path().join("ripgrep"), "").unwrap();
        assert_eq!(pick(dir.path(), "ripgrep", None).unwrap(), dir.path().join("ripgrep"));
        std::fs::write(dir.path().join("other"), "").unwrap();
        assert_eq!(pick(dir.path(), "x", Some("rg")).unwrap(), dir.path().join("rg"));
        assert!(pick(dir.path(), "x", None).is_err());
        assert!(pick(dir.path(), "x", Some("nope")).is_err());
    }
}
