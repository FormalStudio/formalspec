//! Applying and planning hosts that are not this machine: ship the spec over ssh, check the
//! machine matches its declaration, run `fspec` there.

pub mod cmd;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use fspec_core::engine;
use fspec_core::lock::{Lock, SHIPPED_LOADS, loads_cache};
use fspec_core::runner::{Output, Runner};
use fspec_core::{Facts, Host, Plan};
use serde::{Deserialize, Serialize};

pub use cmd::{Target, target};

/// What this fspec can read, compared against the remote's before shipping it a spec. Bump it
/// with every builtin, builtin argument, resource field or lock key a spec can use, since an
/// older fspec fails on them, or worse, ignores an unknown lock key.
pub const ENGINE: u32 = 12;

/// What `fspec facts --json` prints: the machine as it is, to compare against its declaration.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Probe {
    pub hostname: String,
    pub platform: String,
    pub distro: String,
    /// The remote fspec's [`ENGINE`]; one from before the field prints none, read as 0.
    #[serde(default)]
    pub engine: u32,
    /// The spec commit `fspec app` last pushed from the machine
    /// (`~/.local/state/fspec/apps.json`). A spec shipped to it must contain that commit, or the
    /// apply would undo an install made there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apps: Option<String>,
}

impl Probe {
    pub fn local(hostname: String) -> Probe {
        let (platform, distro) = Facts::probe_local();
        let apps = std::env::var_os("HOME")
            .and_then(|h| std::fs::read(Path::new(&h).join(".local/state/fspec/apps.json")).ok())
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .and_then(|v| v.get("commit")?.as_str().map(str::to_string));
        Probe { hostname, platform, distro, engine: ENGINE, apps }
    }
}

/// Fails when the machine is not what the host declares.
pub fn verify(host: &Host, probe: &Probe) -> Result<()> {
    let facts = Facts::declared(&host.name, &host.platform, host.system.as_ref())?;
    let declared = (facts.platform(), facts.distro);
    if declared != (probe.platform.clone(), probe.distro.clone()) {
        bail!(
            "host {} is declared as {}/{} but the machine is {}/{}",
            host.name,
            declared.0,
            declared.1,
            probe.platform,
            probe.distro
        );
    }
    Ok(())
}

/// Only the platform, for `fspec sync`, which touches only the home: a host declared as the
/// distro it is moving to still syncs from the one it runs.
pub fn verify_platform(host: &Host, probe: &Probe) -> Result<()> {
    let facts = Facts::declared(&host.name, &host.platform, host.system.as_ref())?;
    if facts.platform() != probe.platform {
        bail!("host {} is declared as {} but the machine is {}", host.name, facts.platform(), probe.platform);
    }
    Ok(())
}

/// Fails when the fspec that printed `probe` is older than this one.
pub fn current(alias: &str, probe: &Probe) -> Result<()> {
    if probe.engine < ENGINE {
        bail!(
            "fspec on {alias} is older than this one (engine {} < {ENGINE}) and may not read this spec; install a current fspec there first",
            probe.engine
        );
    }
    Ok(())
}

/// The CI build of the locked engine for `platform`, which an older remote gets before it is
/// handed the spec. Its own applies keep it current from then on.
pub fn locked_engine<'l>(lock: &'l Lock, platform: &str) -> Option<&'l str> {
    let source = engine::pkg().source.to_string();
    lock.named(engine::NAME).find(|p| p.source == source)?.artifacts.get(platform)?.oci.as_deref()
}

pub enum Reach {
    Reachable(Probe),
    Unreachable(String),
}

pub enum Planned {
    Plan(Plan),
    Unreachable,
}

pub struct Remote<'a> {
    pub runner: &'a dyn Runner,
    pub spec_dir: PathBuf,
    /// The entry point's file name inside `spec_dir`.
    pub spec_file: String,
    /// Ship a tree with uncommitted changes anyway.
    pub dirty: bool,
    /// Where the `gh:` load files are cached, default [`loads_cache`].
    pub loads_cache: Option<PathBuf>,
    /// Set when `spec_dir` is a [`snapshot`] of this commit (`--rev`): shipped whole, without
    /// asking git about it.
    pub commit: Option<String>,
    /// Queue behind an apply already running on the remote rather than fail (`--no-wait`).
    pub wait: bool,
    /// The git checkout `spec_dir` was snapshotted from, when it is a snapshot.
    pub checkout: Option<PathBuf>,
}

/// One commit of the spec's repo, unpacked on its own (`--rev`).
#[derive(Debug)]
pub struct Snapshot {
    /// The spec dir as that commit has it.
    pub dir: PathBuf,
    pub commit: String,
}

/// Unpack the spec dir as `rev` of its repo has it into `<cache>/<commit>`, so applying it never
/// reads an uncommitted edit. A snapshot is never changed once it is in place, so applies of
/// one commit share it.
pub fn snapshot(runner: &dyn Runner, spec_dir: &Path, rev: &str, cache: &Path) -> Result<Snapshot> {
    let out = runner.run(&cmd::git_rev(spec_dir, rev))?;
    if out.status != 0 {
        bail!("{rev} is not a commit of the repo holding {}: {}", spec_dir.display(), out.stderr.trim());
    }
    let commit = out.stdout.trim().to_string();
    let dir = cache.join(&commit);
    if dir.is_dir() {
        return Ok(Snapshot { dir, commit });
    }
    std::fs::create_dir_all(cache).with_context(|| format!("creating {}", cache.display()))?;
    let tmp = tempfile::tempdir_in(cache)?;
    let archive = tmp.path().join("spec.tar");
    let tree = tmp.path().join("tree");
    std::fs::create_dir(&tree)?;
    runner.check(&cmd::git_archive(spec_dir, &commit, &archive))?;
    runner.check(&cmd::tar_extract(&archive, &tree))?;
    // Another apply of the same commit may have put its copy in place first; either will do.
    if std::fs::rename(&tree, &dir).is_err() && !dir.is_dir() {
        bail!("could not move the snapshot of {commit} into {}", dir.display());
    }
    Ok(Snapshot { dir, commit })
}

/// `<cache>/rev`, where [`snapshot`]s go.
pub fn snapshots() -> Option<PathBuf> {
    Some(fspec_core::lock::cache_dir()?.join("rev"))
}

/// Runs `fspec facts --json` on `alias`.
pub fn probe(runner: &dyn Runner, alias: &str) -> Result<Reach> {
    found(runner, alias)?.with_context(|| {
        format!(
            "fspec is not installed on {alias} (not on PATH for a non-interactive ssh shell); install it there first, e.g. `cargo install --git https://github.com/FormalStudio/formalspec fspec`"
        )
    })
}

/// [`probe`], with `None` for a remote that has no fspec at all.
fn found(runner: &dyn Runner, alias: &str) -> Result<Option<Reach>> {
    let out = runner.run(&cmd::probe(alias))?;
    match out.status {
        0 => Ok(Some(Reach::Reachable(
            serde_json::from_str(&out.stdout)
                .with_context(|| format!("`fspec facts --json` on {alias} printed: {}", out.stdout.trim()))?,
        ))),
        255 => Ok(Some(Reach::Unreachable(out.stderr.trim().to_string()))),
        127 => Ok(None),
        _ => bail!("`fspec facts --json` on {alias} failed: {}", failure(&out)),
    }
}

/// How a remote's fspec is described in the messages of an engine install.
fn runs(engine: Option<u32>) -> String {
    match engine {
        Some(n) => format!("runs engine {n}"),
        None => "has no fspec".into(),
    }
}

impl Remote<'_> {
    /// Probes `alias` and checks it is the machine `host` declares, running an fspec that reads
    /// what this one does. A remote with no fspec at all gets the locked one first, for the
    /// platform the host declares, and is checked once it can answer.
    fn reach(&self, host: &Host, alias: &str) -> Result<Reach> {
        let (platform, engine) = match found(self.runner, alias)? {
            Some(Reach::Reachable(found)) => {
                verify(host, &found)?;
                if found.engine >= ENGINE {
                    return Ok(Reach::Reachable(found));
                }
                (found.platform, Some(found.engine))
            }
            Some(unreachable) => return Ok(unreachable),
            None => (Facts::declared(&host.name, &host.platform, host.system.as_ref())?.platform(), None),
        };
        let lock = Lock::load(&self.spec_dir.join("fspec.lock"))?;
        let tmp = tempfile::tempdir()?;
        let dest = tmp.path().join("pkg");
        match locked_engine(&lock, &platform) {
            Some(reference) => {
                eprintln!("fspec: {alias} {}, installing the locked fspec there first", runs(engine));
                fspec_store::pull_built(self.runner, reference, &tmp.path().join("oci"), &dest, engine::NAME)
                    .context("pulling the locked fspec, which needs oras on this machine's PATH")?;
            }
            None => self.build_engine(host, alias, &platform, engine, &lock, tmp.path(), &dest)?,
        }
        // A build recipe installs into $FSPEC_OUT/bin, a detected one packs at the top.
        let exe = [dest.join("bin").join(engine::NAME), dest.join(engine::NAME)]
            .into_iter()
            .find(|p| p.is_file())
            .with_context(|| format!("the locked fspec build holds no {} executable", engine::NAME))?;
        self.runner.check(&cmd::upload_engine(alias, &exe))?;
        let out = self.runner.run(&cmd::install_engine(alias))?;
        if out.status != 0 {
            bail!("installing fspec on {alias} exited {}", out.status);
        }
        let reach = probe(self.runner, alias)?;
        if let Reach::Reachable(found) = &reach {
            verify(host, found)?;
            current(alias, found)?;
        }
        Ok(reach)
    }

    /// Has an older fspec build the locked engine, for a lock CI has not recorded a build in yet,
    /// and unpacks it into `dest`: the host's builders' own fspec in their order, or the host's
    /// when it names none, since a host that names builders is one that should not compile.
    #[allow(clippy::too_many_arguments)]
    fn build_engine(
        &self,
        host: &Host,
        alias: &str,
        platform: &str,
        engine: Option<u32>,
        lock: &Lock,
        tmp: &Path,
        dest: &Path,
    ) -> Result<()> {
        let pkg = engine::pkg();
        let rev = lock.pkg(engine::NAME, &pkg.source.to_string()).and_then(|p| p.rev.clone()).with_context(|| {
            let has = match engine {
                Some(n) => format!("is older than this one (engine {n} < {ENGINE})"),
                None => "is not installed".into(),
            };
            format!(
                "fspec on {alias} {has} and fspec.lock pins no fspec; run `fspec update fspec`, or install a current fspec there first"
            )
        })?;
        let job = tmp.join("job.json");
        // What `fspec build --job` reads (fspec_build::builders::JobFile).
        std::fs::write(&job, serde_json::to_vec(&serde_json::json!({"pkg": pkg, "rev": rev}))?)?;
        let routes: Vec<&str> = match host.builders.as_slice() {
            [] => vec![alias],
            builders => builders.iter().flat_map(|b| b.routes()).collect(),
        };
        let archive = tmp.join("engine.tar.zst");
        let mut why = Vec::new();
        for route in routes {
            eprintln!(
                "fspec: {alias} {}, and fspec.lock has no CI build of fspec for {platform}, so {route} builds the locked one first",
                runs(engine)
            );
            let built = self
                .runner
                .check(&cmd::upload_engine_job(route, &job))
                .and_then(|_| self.runner.check(&cmd::build_engine(route, platform)))
                .and_then(|_| self.runner.check(&cmd::fetch_engine_build(route, platform, &archive)));
            let _ = self.runner.run(&cmd::clean_engine_job(route));
            match built {
                Ok(_) => return fspec_store::unpack_built(&archive, dest, engine::NAME),
                Err(e) => why.push(format!("{route}: {e:#}")),
            }
        }
        bail!("building the locked fspec for {alias}: {}", why.join("; "))
    }

    /// Copies the git-tracked files of the spec dir (and the untracked ones when it is dirty), plus
    /// `fspec.lock`, to the remote, and the
    /// `gh:` load files this machine has cached, so the remote evaluates without fetching them
    /// (it may have no GitHub token for a private repo). Returns the remote directory as a shell
    /// word.
    pub fn ship(&self, alias: &str) -> Result<String> {
        let (commit, tree_dirty, files) = match &self.commit {
            Some(commit) => (commit.clone(), false, vec![".".to_string()]),
            None => self.tracked()?,
        };
        let content = tree_dirty.then(|| content_hash(&self.spec_dir, &files)).transpose()?;
        let dest = cmd::spec_dir(&commit, content.as_deref());
        let tmp = tempfile::tempdir()?;
        let archive = tmp.path().join("spec.tar");
        self.runner.check(&cmd::tar_create(&self.spec_dir, &archive, &files))?;
        self.runner.check(&cmd::ship(alias, &commit, &dest, &archive, tree_dirty))?;
        let lock = Lock::load(&self.spec_dir.join("fspec.lock"))?;
        if let Some(cache) = self.loads_cache.clone().or_else(loads_cache) {
            let loads: Vec<String> = lock
                .loads
                .iter()
                .flat_map(|(key, l)| l.rel_paths(key))
                .filter(|rel| cache.join(rel).is_file())
                .map(|rel| rel.display().to_string())
                .collect();
            if !loads.is_empty() {
                let archive = tmp.path().join("loads.tar");
                self.runner.check(&cmd::tar_create(&cache, &archive, &loads))?;
                self.runner.check(&cmd::ship_into(alias, &format!("{dest}/{SHIPPED_LOADS}"), &archive))?;
            }
        }
        Ok(dest)
    }

    /// The commit, whether the tree is dirty, and the files of the working tree to ship.
    fn tracked(&self) -> Result<(String, bool, Vec<String>)> {
        let git = |c| self.runner.check(&c).context("the spec must live in a git repository to be shipped");
        let commit = git(cmd::git_commit(&self.spec_dir))?.stdout.trim().to_string();
        let tree_dirty = !git(cmd::git_status(&self.spec_dir))?.stdout.trim().is_empty();
        if tree_dirty && !self.dirty {
            bail!(
                "the spec tree has uncommitted changes; pass --rev HEAD to ship only what is committed, or --dirty to ship them too"
            );
        }
        let mut files: Vec<String> = git(cmd::git_files(&self.spec_dir))?.stdout.lines().map(str::to_string).collect();
        if tree_dirty {
            files.extend(git(cmd::git_untracked(&self.spec_dir))?.stdout.lines().map(str::to_string));
            files.retain(|f| self.spec_dir.join(f).exists());
        }
        if self.spec_dir.join("fspec.lock").exists() && !files.iter().any(|f| f == "fspec.lock") {
            files.push("fspec.lock".into());
        }
        Ok((commit, tree_dirty, files))
    }

    pub fn apply(&self, host: &Host, alias: &str, scope: Option<&str>) -> Result<()> {
        match self.reach(host, alias)? {
            Reach::Unreachable(why) => bail!("host {} is unreachable over ssh ({alias}): {why}", host.name),
            Reach::Reachable(found) => self.has_apps(host, &found)?,
        }
        let dest = self.ship(alias)?;
        let out = self.runner.run(&cmd::apply(alias, &dest, &self.spec_file, &host.name, scope, self.wait))?;
        if out.status != 0 {
            bail!("apply on {} exited {}", host.name, out.status);
        }
        Ok(())
    }

    /// Refuses a spec that lacks the commit `fspec app` last pushed from the host.
    fn has_apps(&self, host: &Host, found: &Probe) -> Result<()> {
        let Some(applied) = &found.apps else { return Ok(()) };
        let repo = self.checkout.as_deref().unwrap_or(&self.spec_dir);
        let ours = match &self.commit {
            Some(c) => c.clone(),
            None => self.runner.check(&cmd::git_commit(repo))?.stdout.trim().to_string(),
        };
        let out = self.runner.run(&cmd::git_contains(repo, applied, &ours))?;
        if out.status != 0 {
            bail!(
                "{} was last changed by `fspec app` at {}, which this checkout does not have; pull first",
                host.name,
                &applied[..12.min(applied.len())]
            );
        }
        Ok(())
    }

    pub fn plan(&self, host: &Host, alias: &str) -> Result<Planned> {
        if let Reach::Unreachable(_) = self.reach(host, alias)? {
            return Ok(Planned::Unreachable);
        }
        let dest = self.ship(alias)?;
        let out = self.runner.check(&cmd::plan(alias, &dest, &self.spec_file, &host.name))?;
        let plan = serde_json::from_str(&out.stdout)
            .with_context(|| format!("`fspec plan --json` on {alias} printed: {}", out.stdout.trim()))?;
        Ok(Planned::Plan(plan))
    }
}

/// The first 12 hex digits of a sha256 over each file's path and bytes, naming a dirty ship so
/// one edit never evaluates another's copy.
fn content_hash(dir: &std::path::Path, files: &[String]) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for f in files {
        let bytes = std::fs::read(dir.join(f)).with_context(|| format!("reading {f} to ship it"))?;
        h.update(f.as_bytes());
        h.update([0]);
        h.update((bytes.len() as u64).to_le_bytes());
        h.update(&bytes);
    }
    Ok(h.finalize().iter().take(6).map(|b| format!("{b:02x}")).collect())
}

fn failure(out: &Output) -> String {
    format!("exit {}: {}", out.status, out.stderr.trim())
}

/// Hosts in the order `--all` applies them: remote hosts as declared, then this machine.
pub fn order<'h>(hosts: &'h [Host], local_name: &str) -> Vec<&'h Host> {
    let (local, remote): (Vec<_>, Vec<_>) = hosts.iter().partition(|h| target(h, local_name) == Target::Local);
    remote.into_iter().chain(local).collect()
}

/// Runs `apply_one` per host in [`order`], stopping at the first failure.
pub fn apply_all(hosts: &[Host], local_name: &str, mut apply_one: impl FnMut(&Host) -> Result<()>) -> Result<()> {
    let ordered = order(hosts, local_name);
    let mut converged = Vec::new();
    for (i, host) in ordered.iter().enumerate() {
        if let Err(e) = apply_one(host) {
            let pending: Vec<_> = ordered[i + 1..].iter().map(|h| h.name.as_str()).collect();
            bail!(
                "host {} failed: {e:#}\nconverged: {}\nnot attempted: {}",
                host.name,
                list(&converged),
                list(&pending)
            );
        }
        converged.push(host.name.as_str());
    }
    Ok(())
}

fn list(names: &[&str]) -> String {
    if names.is_empty() { "none".into() } else { names.join(", ") }
}
