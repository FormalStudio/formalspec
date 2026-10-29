//! `fspec dev`: a project's shell from its `dev.star`.

use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use fspec_core::facts::Facts;
use fspec_core::fetch::LiveFetch;
use fspec_core::runner::LiveRunner;
use fspec_core::{Lock, Paths};
use fspec_dev::{Activated, Machine, Project, libs, services};

#[derive(Subcommand)]
pub enum DevCommand {
    /// Print bash code that sets the project's environment; direnv's `use fspec` runs it.
    Export,
    /// Start the project's services and leave them running.
    Up,
    /// Stop the project's services.
    Down,
    /// Show what the project's environment holds and what fspec.lock could not resolve.
    Plan,
    /// Print the direnv library that defines `use fspec`, for ~/.config/direnv/lib/fspec.sh.
    Direnv,
}

pub fn project(dir: Option<&Path>) -> Result<Project> {
    let from = match dir {
        Some(d) => std::path::absolute(d)?,
        None => std::env::current_dir()?,
    };
    Project::find(&from).with_context(|| format!("no {} at or above {}", fspec_dev::FILE, from.display()))
}

pub(crate) fn machine<'a>(paths: &'a Paths, fetch: &'a LiveFetch) -> Machine<'a> {
    let (platform, _) = Facts::probe_local();
    let manager = if cfg!(target_os = "macos") {
        Some("brew")
    } else {
        std::fs::read_to_string("/etc/os-release").ok().and_then(|t| libs::manager_of_os_release(&t))
    };
    Machine {
        paths,
        runner: &LiveRunner,
        fetch,
        platform,
        manager,
        base: std::env::vars().collect(),
        runtime: fspec_dev::runtime(&LiveRunner),
    }
}

pub fn run(paths: &Paths, dir: Option<&Path>, cmd: Option<&DevCommand>, args: &[String]) -> Result<()> {
    let project = project(dir)?;
    let fetch = LiveFetch::new();
    let m = machine(paths, &fetch);
    let state = services::State::new(&paths.home, &project);
    match cmd {
        Some(DevCommand::Direnv) => {
            print!("{}", fspec_dev::DIRENV_LIB);
            Ok(())
        }
        Some(DevCommand::Plan) => plan(&project, &m),
        Some(DevCommand::Down) => {
            state.mark_up(false)?;
            state.stop(m.runner, m.runtime.as_deref())
        }
        Some(DevCommand::Export) => {
            let a = fspec_dev::activate(&project, &m)?;
            if let Some(image) = &a.dev.container {
                bail!("{} runs in the container {image}; open it with `fspec dev`", project.file().display());
            }
            print!("{}", fspec_dev::export(&m.base, &a.env));
            Ok(())
        }
        Some(DevCommand::Up) => {
            let a = fspec_dev::activate(&project, &m)?;
            services::check_names(&a.dev.services)?;
            state.mark_up(true)?;
            state.start(&a.dev.services, &a.lock, &a.env, &project.dir, m.runner, m.runtime.as_deref())
        }
        None => {
            let a = fspec_dev::activate(&project, &m)?;
            services::check_names(&a.dev.services)?;
            let has_services = !a.dev.services.is_empty();
            if has_services {
                state.enter()?;
                state.start(&a.dev.services, &a.lock, &a.env, &project.dir, m.runner, m.runtime.as_deref())?;
            }
            let status = shell(&a, &m.base, args);
            if has_services && state.leave()? {
                state.stop(m.runner, m.runtime.as_deref())?;
            }
            let code = status?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
    }
}

/// Run `args` (or the user's shell) in the project's environment and return its exit code.
fn shell(a: &Activated, base: &BTreeMap<String, String>, args: &[String]) -> Result<i32> {
    let mut cmd = match &a.run {
        Some(run) => run.command(args, std::io::stdin().is_terminal()),
        None => {
            let program = match args.first() {
                Some(p) => p.clone(),
                None => base.get("SHELL").cloned().unwrap_or_else(|| "sh".into()),
            };
            let mut c = Command::new(program);
            c.args(args.iter().skip(1)).env_clear().envs(&a.env);
            c
        }
    };
    let mut child = cmd.spawn().with_context(|| format!("starting {}", args.first().map_or("the shell", |s| s)))?;
    // The terminal's Ctrl-C reaches fspec too; it waits for the shell instead, so the services
    // still stop when the shell exits. The child was spawned first, so it keeps the default.
    // SAFETY: setting a signal disposition to SIG_IGN and back.
    let old = unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) };
    let status = child.wait();
    // SAFETY: as above.
    unsafe { libc::signal(libc::SIGINT, old) };
    Ok(status?.code().unwrap_or(1))
}

fn plan(project: &Project, m: &Machine) -> Result<()> {
    let dev = project.evaluate(&m.platform)?;
    let lock = Lock::load(&project.lock_path())?;
    println!("{} ({})", project.file().display(), m.platform);
    if !dev.platforms.contains(&m.platform) {
        println!("  this project does not support {}; it targets {}", m.platform, dev.platforms.join(", "));
        return Ok(());
    }
    for p in &dev.packages {
        match lock.pkg(&p.name, &p.source.to_string()) {
            Some(l) => {
                println!("  pkg {} {}", p.name, l.version.as_deref().or(l.rev.as_deref()).unwrap_or("(unversioned)"))
            }
            None => println!("  ! pkg {} is not in fspec.lock, run fspec update", p.name),
        }
    }
    let pinned =
        |image: &str| lock.image.get(image).cloned().unwrap_or_else(|| "(not pinned, run fspec update)".into());
    let misses = fspec_dev::native_misses(&dev, &lock, m.manager);
    let image = match (&dev.container, &dev.fallback_container) {
        (Some(image), _) => Some(image.clone()),
        (None, Some(fallback)) if !misses.is_empty() => {
            println!("  {}: the shell runs in fallback_container {fallback}", misses.join("; "));
            Some(fallback.clone())
        }
        (None, _) => {
            for miss in &misses {
                println!("  ! {miss}: fspec dev fails here; set fallback_container or name the package by hand");
            }
            None
        }
    };
    let manager = match &image {
        Some(image) => {
            println!("  container {image} {}", pinned(image));
            libs::manager_of_image(image)
        }
        None => m.manager,
    };
    for (kind, list) in [("lib", &dev.libs), ("bin", &dev.bins)] {
        for n in list {
            let Some(l) = lock.native(kind, &n.name) else {
                println!("  ! {kind} {} is not in fspec.lock, run fspec update", n.name);
                continue;
            };
            let here = manager.and_then(|mg| l.provides.get(mg).map(|p| format!("{mg} {p}")));
            println!("  {kind} {}: {}", n.name, here.unwrap_or_else(|| "no package for this system".into()));
            for (mg, why) in &l.miss {
                println!("    {mg}: {why}; name it with {kind}(\"{}\", {mg} = \"...\")", n.name);
            }
        }
    }
    for s in &dev.services {
        let what = match (&s.exec, s.image_ref()) {
            (Some(exec), _) => exec.clone(),
            (None, Some(r)) => format!("{r} {}", lock.image.get(&r).map_or("(not pinned, run fspec update)", |s| s)),
            (None, None) => String::new(),
        };
        println!("  service {}: {what}", s.name);
    }
    Ok(())
}

/// `fspec update` in a project directory.
pub fn update(paths: &Paths, only: &[String]) -> Result<()> {
    let project = project(None)?;
    let (fetch, runner) = (LiveFetch::new(), LiveRunner);
    let cx = fspec_sources::Cx::new(&fetch, &runner).with_spec_dir(&project.dir);
    let old = Lock::load(&project.lock_path())?;
    let u = fspec_dev::update(&project, only, &cx, paths)?;
    for line in &u.changes {
        println!("{line}");
    }
    for line in &u.misses {
        eprintln!("fspec: {line}");
    }
    if u.lock != old {
        u.lock.save(&project.lock_path())?;
    }
    if !u.errors.is_empty() {
        bail!("{} problem(s):\n{}", u.errors.len(), u.errors.join("\n"));
    }
    Ok(())
}

/// The spec a command runs, by priority: `--spec`, a spec.star in the current directory, the
/// dev.star project at or above it (`None`, only for `fspec plan` and `fspec update`), then
/// `$FSPEC_SPEC`, then the spec.star that is not there. A directory stands for its spec.star.
pub fn pick_spec(flag: Option<PathBuf>, env: Option<PathBuf>, cwd: &Path, for_project: bool) -> Option<PathBuf> {
    let file = |p: PathBuf| if p.is_dir() { p.join("spec.star") } else { p };
    if let Some(flag) = flag {
        return Some(file(flag));
    }
    let local = cwd.join("spec.star");
    if local.exists() {
        return Some(PathBuf::from("spec.star"));
    }
    if for_project && Project::find(cwd).is_some() {
        return None;
    }
    Some(env.map_or_else(|| PathBuf::from("spec.star"), file))
}

pub fn plan_here(paths: &Paths) -> Result<()> {
    let fetch = LiveFetch::new();
    plan(&project(None)?, &machine(paths, &fetch))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(files: &[&str]) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for f in files {
            std::fs::write(d.path().join(f), "").unwrap();
        }
        d
    }

    #[test]
    fn flag_beats_everything() {
        let d = dir(&["spec.star", "dev.star"]);
        let got = pick_spec(Some("/x/spec.star".into()), Some("/y".into()), d.path(), true);
        assert_eq!(got, Some("/x/spec.star".into()));
    }

    #[test]
    fn a_directory_stands_for_its_spec() {
        let d = dir(&[]);
        let got = pick_spec(Some(d.path().into()), None, Path::new("/nowhere"), false);
        assert_eq!(got, Some(d.path().join("spec.star")));
    }

    #[test]
    fn local_spec_beats_project_and_env() {
        let d = dir(&["spec.star", "dev.star"]);
        let got = pick_spec(None, Some("/y/spec.star".into()), d.path(), true);
        assert_eq!(got, Some("spec.star".into()));
    }

    #[test]
    fn project_beats_env_for_plan_and_update_only() {
        let d = dir(&["dev.star"]);
        let env = Some(PathBuf::from("/sys/spec.star"));
        assert_eq!(pick_spec(None, env.clone(), d.path(), true), None);
        assert_eq!(pick_spec(None, env, d.path(), false), Some("/sys/spec.star".into()));
    }

    #[test]
    fn env_then_spec_star() {
        let d = dir(&[]);
        assert_eq!(pick_spec(None, Some("/sys/spec.star".into()), d.path(), true), Some("/sys/spec.star".into()));
        assert_eq!(pick_spec(None, None, d.path(), true), Some("spec.star".into()));
    }
}
