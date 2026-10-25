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
}

#[derive(Debug)]
pub struct Built {
    /// `<out>/<name>-<platform>.tar.zst`
    pub archive: PathBuf,
    /// The registry tag the workflow pushes it under, `<rev>-<platform>`; also written to
    /// `<out>/tag`.
    pub tag: String,
}

/// Fetch the locked commit, apply the patches, run the recipe and pack the executables.
/// Everything external goes through `runner`.
pub fn build(runner: &dyn Runner, job: &Job) -> Result<Built> {
    let work = tempfile::tempdir().context("creating a build directory")?;
    let dirs = Dirs { src: work.path().join("src"), build: work.path().join("build"), out: work.path().join("out") };
    let src = dirs.src.display().to_string();
    for cmd in [
        Cmd::new("git").args(["init", "-q", &src]),
        Cmd::new("git").args(["-C", &src, "remote", "add", "origin", job.url]),
        Cmd::new("git").args(["-C", &src, "fetch", "-q", "--depth", "1", "origin", job.rev]),
        Cmd::new("git").args(["-C", &src, "checkout", "-q", "FETCH_HEAD"]),
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

    fs::create_dir_all(&dirs.out)?;
    let recipe = recipe::select(&dirs.src, &job.pkg.build)?;
    let commands = recipe.commands(&dirs, &job.pkg.build_flags, &job.pkg.build);
    let commands = match &job.pkg.source {
        Source::Git { image: Some(image), deps, .. } => {
            vec![in_container(runner, image, deps, work.path(), &dirs.src, &commands)]
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
    let tag = format!("{}-{}", job.rev, job.platform);
    fs::write(job.out.join("tag"), &tag)?;
    Ok(Built { archive, tag })
}

/// The recipe as one `podman run` (or `docker run` where podman is missing) of `image`: the work
/// directory is mounted at its own path, so every path in the commands holds inside, and `deps`
/// install first with whichever package manager the image has.
fn in_container(runner: &dyn Runner, image: &str, deps: &[String], work: &Path, src: &Path, commands: &[Cmd]) -> Cmd {
    let podman = runner.run(&Cmd::new("sh").args(["-c", "command -v podman"])).is_ok_and(|o| o.status == 0);
    let work = work.display().to_string();
    let mut run = Cmd::new(if podman { "podman" } else { "docker" })
        .args(["run", "--rm", "-v"])
        .arg(format!("{work}:{work}:Z"))
        .arg("-w")
        .arg(src.display().to_string());
    let mut script = vec!["set -e".to_string()];
    if !deps.is_empty() {
        script.push(install(deps));
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

fn install(deps: &[String]) -> String {
    let deps = shell_words::join(deps);
    format!(
        "if command -v zypper >/dev/null; then zypper --non-interactive --gpg-auto-import-keys install --no-recommends {deps}\n\
         elif command -v dnf >/dev/null; then dnf install -y {deps}\n\
         elif command -v apt-get >/dev/null; then apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends {deps}\n\
         elif command -v pacman >/dev/null; then pacman -Syu --noconfirm --needed {deps}\n\
         elif command -v xbps-install >/dev/null; then xbps-install -Syu {deps}\n\
         elif command -v apk >/dev/null; then apk add {deps}\n\
         else echo 'the build image has none of zypper, dnf, apt-get, pacman, xbps-install, apk' >&2; exit 1; fi"
    )
}

#[cfg(test)]
mod tests;
