//! Building a `git(...)` package that CI has no artifact for: on the host's builders over ssh,
//! or on the host itself when it declares none. Each builder is probed once per run over its routes in order, and
//! the first route that answers is kept for the rest of the run, so a builder that is down costs
//! one connect timeout per route per run, not one per package.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use fspec_core::patches::patch_path;
use fspec_core::resource::{Builder, Pkg};
use fspec_core::runner::{Cmd, Runner};
use serde::{Deserialize, Serialize};

use crate::run::{Job, build};

/// Builder name to the route this run reaches it by, `None` when none answered.
static ROUTES: Mutex<BTreeMap<String, Option<String>>> = Mutex::new(BTreeMap::new());

/// What `fspec build --job` reads on a builder: the package as the host's spec declares it, at
/// the host's locked rev. Its patches sit beside the job file, at their spec-relative paths.
#[derive(Serialize, Deserialize)]
pub struct JobFile {
    pub pkg: Pkg,
    pub rev: String,
}

/// Build `job` on the first builder that is reachable, runs the job's platform and succeeds,
/// and return the archive under `job.out`. Only a host without builders builds it itself: a host
/// that names builders is one that should not compile.
pub fn build_anywhere(runner: &dyn Runner, builders: &[Builder], job: &Job) -> Result<PathBuf> {
    if builders.is_empty() {
        return Ok(build(runner, job)?.archive);
    }
    let mut why = Vec::new();
    for b in builders {
        let Some(route) = route(runner, b, job.platform) else {
            why.push(format!("{} is unreachable or not {}", b.name, job.platform));
            continue;
        };
        match remote(runner, &b.name, &route, job) {
            Ok(archive) => return Ok(archive),
            Err(e) => why.push(format!("{} ({route}): {e:#}", b.name)),
        }
    }
    bail!("no builder built {}: {}", job.pkg.name, why.join("; "))
}

/// ssh to `route`, as the user who ran sudo when fspec runs as root through it: root's own
/// ssh has none of the user's keys or known hosts.
fn ssh(route: &str) -> Cmd {
    // A build runs for minutes with no output on the wire; keepalives turn a dropped link into an
    // error instead of a hang.
    let opts = ["-o", "ConnectTimeout=5", "-o", "BatchMode=yes", "-o", "ServerAliveInterval=15"];
    let cmd = match std::env::var("SUDO_USER").ok().filter(|_| is_root()) {
        Some(user) => Cmd::new("sudo").args(["-u", &user, "-H", "--preserve-env=SSH_AUTH_SOCK", "ssh"]),
        None => Cmd::new("ssh"),
    };
    cmd.args(opts).arg(route)
}

fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

/// The builder's route for this run, probing it the first time.
fn route(runner: &dyn Runner, b: &Builder, platform: &str) -> Option<String> {
    if let Some(known) = ROUTES.lock().unwrap().get(&b.name) {
        return known.clone();
    }
    let mut found = None;
    for r in b.routes() {
        let Ok(out) = runner.run(&ssh(r).arg("uname -sm")) else { continue };
        if out.status != 0 {
            continue;
        }
        match platform_of(&out.stdout) {
            Some(p) if p == platform => found = Some(r.to_string()),
            p => eprintln!(
                "fspec: builder {} runs {}, not {platform}",
                b.name,
                p.as_deref().unwrap_or("an unknown platform")
            ),
        }
        break;
    }
    if found.is_none() {
        eprintln!("fspec: builder {} is not reachable over {}", b.name, b.routes().join(", "));
    }
    ROUTES.lock().unwrap().insert(b.name.clone(), found.clone());
    found
}

/// `uname -sm` as a host platform: `Linux x86_64` is `x86_64-linux`.
fn platform_of(uname: &str) -> Option<String> {
    let mut words = uname.split_whitespace();
    let os = match words.next()? {
        "Linux" => "linux",
        "Darwin" => "darwin",
        _ => return None,
    };
    let arch = match words.next()? {
        "arm64" => "aarch64",
        a => a,
    };
    Some(format!("{arch}-{os}"))
}

/// Ship this fspec, the job and the package's patches to `route`, run `fspec build --job` there
/// and copy the archive back.
fn remote(runner: &dyn Runner, name: &str, route: &str, job: &Job) -> Result<PathBuf> {
    let rev: String = job.rev.chars().take(12).collect();
    let stage = tempfile::tempdir()?;
    // Each job gets its own directory: two applies building one rev once shared it, and the
    // second's unpack deleted the first's build under it. The builder's build cache, not this
    // directory, is what carries between jobs.
    let id = stage
        .path()
        .file_name()
        .context("a temp dir has a name")?
        .to_string_lossy()
        .trim_start_matches('.')
        .to_string();
    let dir = format!(".cache/fspec/jobs/{}-{rev}-{id}", job.pkg.name);
    let exe = std::env::current_exe().context("finding this fspec to ship to the builder")?;
    fs::copy(&exe, stage.path().join("fspec")).context("staging fspec for the builder")?;
    let file = JobFile { pkg: job.pkg.clone(), rev: job.rev.to_string() };
    fs::write(stage.path().join("job.json"), serde_json::to_vec(&file)?)?;
    for patch in &job.pkg.patches {
        let to = stage.path().join(patch);
        fs::create_dir_all(to.parent().context("a patch path has a parent")?)?;
        fs::copy(patch_path(job.spec_dir, patch)?, &to).with_context(|| format!("staging {patch}"))?;
    }
    let tar = stage.path().with_extension("tar");
    runner
        .check(&Cmd::new("tar").arg("-cf").arg(tar.display().to_string()).arg("-C").arg(path(stage.path())).arg("."))?;
    let unpack = format!("mkdir -p {dir} && tar -xf - -C {dir}");
    let shipped = runner.check(&ssh(route).arg(unpack).stdin_file(&tar));
    let _ = fs::remove_file(&tar);
    shipped.with_context(|| format!("shipping the build to {name}"))?;

    let archive_name = format!("{}-{}.tar.zst", job.pkg.name, job.platform);
    let run = format!("cd {dir} && ./fspec build --job job.json --platform {} --out out", job.platform);
    let built = runner.check(&ssh(route).arg(run));
    let archive = job.out.join(&archive_name);
    let fetched = built.and_then(|_| {
        fs::create_dir_all(job.out)?;
        let ssh = ssh(route).arg(format!("cat {dir}/out/{archive_name}"));
        runner.check(
            &Cmd::new("sh")
                .args(["-c", r#"out=$1; shift; "$@" > "$out""#, "sh", &path(&archive)])
                .arg(ssh.program)
                .args(ssh.args),
        )
    });
    let _ = runner.run(&ssh(route).arg(format!("rm -rf {dir}")));
    fetched?;
    if !archive.is_file() || fs::metadata(&archive)?.len() == 0 {
        bail!("{name} returned no {archive_name}");
    }
    Ok(archive)
}

fn path(p: &Path) -> String {
    p.display().to_string()
}

#[cfg(test)]
mod tests {
    use fspec_core::runner::{Output, RecordingRunner};

    use super::*;

    const SSH: &str = "ssh -o ConnectTimeout=5 -o BatchMode=yes -o ServerAliveInterval=15";

    fn pkg(name: &str) -> Pkg {
        serde_json::from_value(
            serde_json::json!({"name": name, "source": {"kind": "git", "url": "https://x/y", "rev": "main"}}),
        )
        .unwrap()
    }

    fn builder(name: &str, routes: &[&str]) -> Builder {
        Builder { name: name.into(), routes: routes.iter().map(|r| r.to_string()).collect() }
    }

    #[test]
    fn uname_names_the_platform() {
        assert_eq!(platform_of("Linux x86_64\n").as_deref(), Some("x86_64-linux"));
        assert_eq!(platform_of("Darwin arm64").as_deref(), Some("aarch64-darwin"));
        assert_eq!(platform_of("FreeBSD amd64"), None);
    }

    #[test]
    fn a_builder_is_probed_once_per_run_and_keeps_the_first_route_that_answers() {
        let runner = RecordingRunner::default();
        runner.respond(&format!("{SSH} ts-probe"), Output { status: 255, ..Output::default() });
        runner.respond(
            &format!("{SSH} lan-probe"),
            Output { status: 0, stdout: "Linux x86_64\n".into(), ..Output::default() },
        );
        let b = builder("probe-once", &["ts-probe", "lan-probe"]);
        assert_eq!(route(&runner, &b, "x86_64-linux").as_deref(), Some("lan-probe"));
        assert_eq!(route(&runner, &b, "x86_64-linux").as_deref(), Some("lan-probe"));
        let probes = runner.lines().iter().filter(|l| l.contains("uname")).count();
        assert_eq!(probes, 2, "{:#?}", runner.lines());
    }

    #[test]
    fn a_builder_of_another_platform_is_skipped() {
        let runner = RecordingRunner::default();
        runner.respond(
            &format!("{SSH} mac-arm"),
            Output { status: 0, stdout: "Darwin arm64\n".into(), ..Output::default() },
        );
        assert_eq!(route(&runner, &builder("mac-arm", &[]), "x86_64-linux"), None);
    }

    #[test]
    fn with_no_builder_reachable_the_host_does_not_build_it_itself() {
        let runner = RecordingRunner::default();
        runner.respond(&format!("{SSH} down-a"), Output { status: 255, ..Output::default() });
        let dir = tempfile::tempdir().unwrap();
        let p = pkg("tool");
        let job = Job {
            pkg: &p,
            url: "https://x/y",
            rev: "abc",
            platform: "x86_64-linux",
            spec_dir: dir.path(),
            out: dir.path(),
            cache: dir.path(),
        };
        let err = build_anywhere(&runner, &[builder("down-a", &[])], &job).unwrap_err();
        assert!(err.to_string().contains("no builder built tool: down-a is unreachable"), "{err:#}");
        assert!(!runner.lines().iter().any(|l| l.starts_with("git ")), "{:#?}", runner.lines());
    }

    #[test]
    fn without_builders_the_host_builds_it_itself() {
        let runner = RecordingRunner::default();
        let dir = tempfile::tempdir().unwrap();
        let p = pkg("tool");
        let job = Job {
            pkg: &p,
            url: "https://x/y",
            rev: "abc",
            platform: "x86_64-linux",
            spec_dir: dir.path(),
            out: dir.path(),
            cache: dir.path(),
        };
        // The recording runner leaves no checkout to select a recipe from, so the build ends there.
        let err = build_anywhere(&runner, &[], &job).unwrap_err();
        assert!(err.to_string().contains("no Cargo.toml"), "{err:#}");
        assert!(runner.lines().iter().any(|l| l.contains("fetch -q --depth 1 origin abc")), "{:#?}", runner.lines());
    }
}
