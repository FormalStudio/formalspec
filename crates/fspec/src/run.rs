//! `fspec run`: one package run without adding it to the spec. It is a one-package `dev.star` in
//! the cache, activated as `fspec dev` activates a project, so its store paths live and go the way
//! a project's do (`fspec gc`). docs/shell-install.md has the design.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use fspec_core::activity::{self, Kind, Operation, Phase};
use fspec_core::fetch::LiveFetch;
use fspec_core::runner::LiveRunner;
use fspec_core::{Lock, Paths};
use fspec_dev::Project;

/// What `fspec run` was asked for: the package's name, and its `pkg(...)` in a `dev.star`.
#[derive(Debug, PartialEq, Eq)]
pub struct Declared {
    pub name: String,
    pub pkg: String,
}

/// `ripgrep`, `aqua:ripgrep`, `store:ripgrep`, `github:sharkdp/bat`, `npm:@scope/tool`,
/// `cargo:xh`. Only store sources run without being installed.
pub fn declare(spec: &str) -> Result<Declared> {
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
        "github" => bail!("github:{arg} must be owner/repo"),
        "flatpak" | "distro" | "zypper" | "pacman" | "dnf" | "apt" | "xbps" | "brew" => {
            bail!("{source} packages cannot run without being installed; install it with `fspec app install {spec}`")
        }
        other => bail!("fspec run takes aqua, github, npm and cargo packages, not {other}"),
    };
    let name = if matches!(source, "aqua" | "store") { arg.to_string() } else { last };
    Ok(Declared { name, pkg })
}

/// Where run projects live: beside the dev link farms, whose gc they share.
pub fn runs_dir(home: &Path) -> PathBuf {
    fspec_dev::cache_dir(home).join("once")
}

pub fn run(paths: &Paths, spec: &str, bin: Option<&str>, update: bool, detach: bool, args: &[String]) -> Result<()> {
    let d = declare(spec)?;
    let (platform, _) = fspec_core::facts::Facts::probe_local();
    let source = format!("dev(packages = [{}], platforms = [{platform:?}])\n", d.pkg);
    let dir = runs_dir(&paths.home).join(fspec_dev::short_hash(&source));
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    // Written on every run: its age is what `fspec gc` reads as the last use.
    std::fs::write(dir.join(fspec_dev::FILE), &source)?;
    let op = Operation::new(Kind::Run, spec, format!("Running {}", d.name), None);
    let child = activity::run(op, || {
        let project = Project { dir: dir.clone() };
        let fetch = LiveFetch::new();
        if update || !project.lock_path().is_file() {
            activity::phase(Phase::Resolving, None);
            let cx = fspec_sources::Cx::new(&fetch, &LiveRunner).with_spec_dir(&dir);
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
        let child = c.spawn().with_context(|| format!("starting {}", d.name))?;
        Ok((!detach).then_some(child))
    })?;
    let Some(mut child) = child else { return Ok(()) };
    // The terminal's Ctrl-C is the command's to handle; fspec only passes its exit status on.
    // SAFETY: setting a signal disposition to SIG_IGN and back.
    let old = unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) };
    let status = child.wait();
    // SAFETY: as above.
    unsafe { libc::signal(libc::SIGINT, old) };
    let code = status.with_context(|| format!("waiting for {}", d.name))?.code().unwrap_or(1);
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
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
        let d = |s: &str| declare(s).unwrap();
        assert_eq!(d("ripgrep"), Declared { name: "ripgrep".into(), pkg: r#"pkg("ripgrep")"#.into() });
        assert_eq!(d("github:sharkdp/bat").pkg, r#"pkg("bat", source = github("sharkdp/bat"))"#);
        assert_eq!(
            d("npm:@scope/tool"),
            Declared { name: "tool".into(), pkg: r#"pkg("tool", source = npm("@scope/tool"))"#.into() }
        );
        assert_eq!(d("cargo:xh").pkg, r#"pkg("xh", source = cargo("xh"))"#);
    }

    #[test]
    fn what_cannot_run_uninstalled_or_could_break_out_of_the_string_is_refused() {
        assert!(declare("flatpak:org.mozilla.firefox").unwrap_err().to_string().contains("fspec app install"));
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
