use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use fspec_core::patches::patch_path;
use fspec_core::resource::{Pkg, Source};
use fspec_core::runner::{Cmd, Runner};

use crate::pack;
use crate::recipe::{self, Dirs, Recipe};

/// One package built for one platform at its locked commit.
pub struct Job<'a> {
    pub pkg: &'a Pkg,
    pub url: &'a str,
    pub rev: &'a str,
    pub platform: &'a str,
    pub spec_dir: &'a Path,
    pub out: &'a Path,
    /// The build cache, [`cache_root`] outside tests. Each package and platform keeps its
    /// checkout, build directory and `FSPEC_CACHE` there between revs, so a rev bump builds
    /// incrementally.
    pub cache: &'a Path,
}

/// `$XDG_CACHE_HOME/fspec/build`, else `~/.cache/fspec/build`.
pub fn cache_root() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .context("neither XDG_CACHE_HOME nor HOME is set")?;
    Ok(base.join("fspec/build"))
}

#[derive(Debug)]
pub struct Built {
    /// `<out>/<name>-<platform>.tar.zst`
    pub archive: PathBuf,
    /// The registry tag the workflow pushes it under, [`crate::tag`]; also written to
    /// `<out>/tag`.
    pub tag: String,
}

/// Fetch the locked commit, apply the patches, run the recipe and pack the executables.
/// Everything external goes through `runner`.
pub fn build(runner: &dyn Runner, job: &Job) -> Result<Built> {
    let cache = job.cache.join(format!("{}-{}", job.pkg.name, job.platform));
    fs::create_dir_all(&cache).with_context(|| format!("creating {}", cache.display()))?;
    // Two applies can build the same package at once; the second waits for the first's cache.
    let lock = fs::File::create(cache.with_extension("lock"))?;
    lock.lock().with_context(|| format!("locking {}", cache.display()))?;
    let user = build_user(job.platform);
    if let Some(u) = &user {
        runner.check(&Cmd::new("chown").args(["-R", u]).arg(cache.display().to_string()))?;
    }
    // Inside the cache, so a build run as the sudo user can write it.
    let work = tempfile::tempdir_in(&cache).context("creating a build directory")?;
    let dirs = Dirs {
        src: cache.join("src"),
        build: cache.join("build"),
        out: work.path().join("out"),
        cache: cache.join("cache"),
    };
    fs::create_dir_all(&dirs.cache)?;
    fs::create_dir_all(&dirs.out)?;
    if let Some(u) = &user {
        for dir in [&dirs.cache, &work.path().to_path_buf()] {
            runner.check(&Cmd::new("chown").args(["-R", u]).arg(dir.display().to_string()))?;
        }
    }
    let runner = &AsUser { inner: runner, user: user.as_deref() };
    let src = dirs.src.display().to_string();
    if !dirs.src.join(".git").is_dir() {
        runner.check(&Cmd::new("git").args(["init", "-q", &src])).map_err(|e| {
            match e.root_cause().downcast_ref::<std::io::Error>() {
                Some(io) if io.kind() == std::io::ErrorKind::NotFound => {
                    anyhow::anyhow!("git is not installed; building `git(...)` packages needs it")
                }
                _ => e,
            }
        })?;
        runner.check(&Cmd::new("git").args(["-C", &src, "remote", "add", "origin", job.url]))?;
    }
    // A forced checkout and a clean drop the last build's patches and untracked files; ignored
    // files (an in-tree target/) stay, so they build incrementally too.
    for cmd in [
        Cmd::new("git").args(["-C", &src, "remote", "set-url", "origin", job.url]),
        Cmd::new("git").args(["-C", &src, "fetch", "-q", "--depth", "1", "origin", job.rev]),
        Cmd::new("git").args(["-C", &src, "checkout", "-q", "-f", "FETCH_HEAD"]),
        Cmd::new("git").args(["-C", &src, "clean", "-q", "-ffd"]),
    ] {
        runner.check(&cmd)?;
    }
    for patch in &job.pkg.patches {
        // `git -C` resolves a relative path against the checkout, and a builder's spec_dir is the
        // job file's own relative directory.
        let file = std::path::absolute(patch_path(job.spec_dir, patch)?)?;
        runner
            .check(&Cmd::new("git").args(["-C", &src, "apply"]).arg(file.display().to_string()))
            .with_context(|| format!("applying {patch}"))?;
    }

    let recipe = recipe::select(&dirs.src, &job.pkg.build)?;
    let commands = recipe.commands(&dirs, &job.pkg.build_flags, &job.pkg.build);
    let commands = match &job.pkg.source {
        Source::Git { image: Some(image), deps, .. } => {
            let pkgs = cache.join("pkgs");
            vec![in_container(runner, image, deps, &pkgs, &[work.path(), &cache], &dirs.src, &commands)]
        }
        // macOS has no container of its own kind to build in; Homebrew is its package manager.
        Source::Git { deps, .. } if !deps.is_empty() && job.platform.ends_with("-darwin") => {
            std::iter::once(Cmd::new("brew").arg("install").args(deps)).chain(commands).collect()
        }
        Source::Git { deps, .. } if !deps.is_empty() => {
            bail!("{} lists deps but no image to install them into, set git(image = ...)", job.pkg.name)
        }
        _ => commands,
    };
    for cmd in commands {
        runner.check(&cmd).with_context(|| format!("building {}", job.pkg.name))?;
    }

    let executables = pack::executables(&recipe.bin_dirs(&dirs), &job.pkg.bins);
    let body = match recipe {
        Recipe::Cargo => pack::tar_zst(&executables?)?,
        // A prefix install keeps its tree: data in share/, helpers in lib/. `bins` must exist.
        _ => {
            if !job.pkg.bins.is_empty() {
                executables?;
            }
            pack::tree_zst(&dirs.out)?
        }
    };
    let archive = job.out.join(format!("{}-{}.tar.zst", job.pkg.name, job.platform));
    fs::create_dir_all(job.out)?;
    fs::write(&archive, body).with_context(|| format!("writing {}", archive.display()))?;
    let tag = job_tag(job)?;
    fs::write(job.out.join("tag"), &tag)?;
    drop(lock);
    Ok(Built { archive, tag })
}

/// The registry tag of `job`, its patches hashed as `fspec update` locks them.
pub fn job_tag(job: &Job) -> Result<String> {
    let patches = match job.pkg.patches.is_empty() {
        true => None,
        false => Some(fspec_core::patches::hash(job.spec_dir, &job.pkg.patches)?),
    };
    Ok(crate::tag(job.rev, &crate::identity(job.pkg, job.url, job.rev, patches.as_deref())?, job.platform))
}

/// Homebrew refuses to run as root, and a macOS recipe installs its tools with it. When the
/// system scope's apply runs through sudo, a macOS build runs as the user who ran sudo.
fn build_user(platform: &str) -> Option<String> {
    // SAFETY: geteuid has no preconditions.
    let root = unsafe { libc::geteuid() == 0 };
    std::env::var("SUDO_USER").ok().filter(|u| root && platform.ends_with("-darwin") && u != "root")
}

struct AsUser<'a> {
    inner: &'a dyn Runner,
    user: Option<&'a str>,
}

impl Runner for AsUser<'_> {
    fn run(&self, cmd: &Cmd) -> Result<fspec_core::runner::Output> {
        match self.user {
            Some(u) => self.inner.run(&as_user(cmd, u)),
            None => self.inner.run(cmd),
        }
    }
}

/// `cmd` through `sudo -u user -H env`, which carries its environment and PATH past sudo's reset.
fn as_user(cmd: &Cmd, user: &str) -> Cmd {
    let mut out = Cmd::new("sudo").args(["-u", user, "-H", "env"]);
    if !cmd.env.iter().any(|(k, _)| k == "PATH")
        && let Ok(path) = std::env::var("PATH")
    {
        out = out.arg(format!("PATH={path}"));
    }
    out = out.args(cmd.env.iter().map(|(k, v)| format!("{k}={v}"))).arg(&cmd.program).args(cmd.args.iter().cloned());
    Cmd { stdin: cmd.stdin.clone(), stdin_file: cmd.stdin_file.clone(), interactive: cmd.interactive, ..out }
}

/// The recipe as one `podman run` (or `docker run` where podman is missing) of `image`: each of
/// `mounts` is mounted at its own path, so every path in the commands holds inside, and `deps`
/// install first with whichever package manager the image has, downloading into `pkgs` (inside
/// one of `mounts`) so the next build of the package fetches only what changed.
fn in_container(
    runner: &dyn Runner,
    image: &str,
    deps: &[String],
    pkgs: &Path,
    mounts: &[&Path],
    src: &Path,
    commands: &[Cmd],
) -> Cmd {
    let podman = runner.run(&Cmd::new("sh").args(["-c", "command -v podman"])).is_ok_and(|o| o.status == 0);
    let mut run = Cmd::new(if podman { "podman" } else { "docker" }).args(["run", "--rm"]);
    for m in mounts {
        let m = m.display().to_string();
        run = run.arg("-v").arg(format!("{m}:{m}:Z"));
    }
    run = run.arg("-w").arg(src.display().to_string());
    let mut script = vec!["set -e".to_string()];
    if !podman {
        // Docker's root is the host's root, so whatever the build writes into the cache would
        // be root's, and the next build's git checkout there could not touch it. Rootless
        // podman's root is the user already.
        // SAFETY: getuid and getgid have no preconditions.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        let dirs: Vec<String> = mounts.iter().map(|m| m.display().to_string()).collect();
        script.push(format!("trap \"chown -R {uid}:{gid} {}\" EXIT", shell_words::join(&dirs)));
    }
    if !deps.is_empty() {
        script.push(install(deps, &pkgs.display().to_string()));
    }
    for cmd in commands {
        for (k, v) in &cmd.env {
            run = run.arg("-e").arg(format!("{k}={v}"));
        }
        let mut words = vec![cmd.program.clone()];
        words.extend(cmd.args.iter().cloned());
        script.push(shell_words::join(&words));
    }
    run.arg(image).args(["sh", "-c"]).arg(script.join("\n"))
}

// Docker's debian and ubuntu images ship apt.conf.d/docker-clean, which deletes every deb after
// it installs; with it gone, apt keeps them in the mounted archives dir.
fn install(deps: &[String], pkgs: &str) -> String {
    let deps = shell_words::join(deps);
    let p = shell_words::quote(pkgs);
    format!(
        "if command -v zypper >/dev/null; then zypper -q mr -k -a && zypper --non-interactive --gpg-auto-import-keys --cache-dir {p}/zypp install --no-recommends {deps}\n\
         elif command -v dnf >/dev/null; then dnf install -y --setopt=keepcache=1 --setopt=cachedir={p}/dnf {deps}\n\
         elif command -v apt-get >/dev/null; then rm -f /etc/apt/apt.conf.d/docker-clean && mkdir -p {p}/apt/partial {p}/apt-lists/partial && \
         apt-get -o Dir::Cache::Archives={p}/apt -o Dir::State::Lists={p}/apt-lists update && \
         DEBIAN_FRONTEND=noninteractive apt-get -o Dir::Cache::Archives={p}/apt -o Dir::State::Lists={p}/apt-lists install -y --no-install-recommends {deps}\n\
         elif command -v pacman >/dev/null; then mkdir -p {p}/pacman && pacman -Syu --noconfirm --needed --cachedir {p}/pacman {deps}\n\
         elif command -v xbps-install >/dev/null; then xbps-install -Syu --cachedir {p}/xbps {deps}\n\
         elif command -v apk >/dev/null; then apk add --cache-dir {p}/apk {deps}\n\
         else echo 'the build image has none of zypper, dnf, apt-get, pacman, xbps-install, apk' >&2; exit 1; fi"
    )
}

#[cfg(test)]
mod tests;
