//! Applying and planning hosts that are not this machine: ship the spec over ssh, check the
//! machine matches its declaration, run `fspec` there.

pub mod cmd;

use std::path::PathBuf;

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
pub const ENGINE: u32 = 7;

/// What `fspec facts --json` prints: the machine as it is, to compare against its declaration.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Probe {
    pub hostname: String,
    pub platform: String,
    pub distro: String,
    /// The remote fspec's [`ENGINE`]; one from before the field prints none, read as 0.
    #[serde(default)]
    pub engine: u32,
}

impl Probe {
    pub fn local(hostname: String) -> Probe {
        let (platform, distro) = Facts::probe_local();
        Probe { hostname, platform, distro, engine: ENGINE }
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
}

/// Runs `fspec facts --json` on `alias`.
pub fn probe(runner: &dyn Runner, alias: &str) -> Result<Reach> {
    let out = runner.run(&cmd::probe(alias))?;
    match out.status {
        0 => Ok(Reach::Reachable(
            serde_json::from_str(&out.stdout)
                .with_context(|| format!("`fspec facts --json` on {alias} printed: {}", out.stdout.trim()))?,
        )),
        255 => Ok(Reach::Unreachable(out.stderr.trim().to_string())),
        127 => bail!(
            "fspec is not installed on {alias} (not on PATH for a non-interactive ssh shell); install it there first, e.g. `cargo install --git https://github.com/FormalStudio/formalspec fspec`"
        ),
        _ => bail!("`fspec facts --json` failed on {alias}: {}", failure(&out)),
    }
}

impl Remote<'_> {
    /// Probes `alias` and checks it is the machine `host` declares, running an fspec that reads
    /// what this one does.
    fn reach(&self, host: &Host, alias: &str) -> Result<Reach> {
        let reach = probe(self.runner, alias)?;
        let Reach::Reachable(found) = &reach else { return Ok(reach) };
        verify(host, found)?;
        if found.engine >= ENGINE {
            return Ok(reach);
        }
        let lock = Lock::load(&self.spec_dir.join("fspec.lock"))?;
        let Some(reference) = locked_engine(&lock, &found.platform) else {
            bail!(
                "fspec on {alias} is older than this one (engine {} < {ENGINE}) and fspec.lock has no CI build of fspec for {} to install; run `fspec update --cache`, or install a current fspec there first",
                found.engine,
                found.platform
            );
        };
        eprintln!("fspec: {alias} runs engine {}, installing the locked fspec there first", found.engine);
        let tmp = tempfile::tempdir()?;
        let dest = tmp.path().join("pkg");
        fspec_store::pull_built(self.runner, reference, &tmp.path().join("oci"), &dest, engine::NAME)
            .context("pulling the locked fspec, which needs oras on this machine's PATH")?;
        // A build recipe installs into $FSPEC_OUT/bin, a detected one packs at the top.
        let exe = [dest.join("bin").join(engine::NAME), dest.join(engine::NAME)]
            .into_iter()
            .find(|p| p.is_file())
            .with_context(|| format!("the locked fspec build {reference} holds no {} executable", engine::NAME))?;
        self.runner.check(&cmd::upload_engine(alias, &exe))?;
        let out = self.runner.run(&cmd::install_engine(alias))?;
        if out.status != 0 {
            bail!("installing fspec on {alias} exited {}", out.status);
        }
        let reach = probe(self.runner, alias)?;
        if let Reach::Reachable(found) = &reach {
            current(alias, found)?;
        }
        Ok(reach)
    }

    /// Copies the git-tracked files of the spec dir (and the untracked ones when it is dirty), plus
    /// `fspec.lock`, to the remote, and the
    /// `gh:` load files this machine has cached, so the remote evaluates without fetching them
    /// (it may have no GitHub token for a private repo). Returns the remote directory as a shell
    /// word.
    pub fn ship(&self, alias: &str) -> Result<String> {
        let git = |c| self.runner.check(&c).context("the spec must live in a git repository to be shipped");
        let commit = git(cmd::git_commit(&self.spec_dir))?.stdout.trim().to_string();
        let tree_dirty = !git(cmd::git_status(&self.spec_dir))?.stdout.trim().is_empty();
        if tree_dirty && !self.dirty {
            bail!(
                "the spec tree has uncommitted changes; commit them so every host applies the same commit, or pass --dirty"
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

    pub fn apply(&self, host: &Host, alias: &str, scope: Option<&str>) -> Result<()> {
        if let Reach::Unreachable(why) = self.reach(host, alias)? {
            bail!("host {} is unreachable over ssh ({alias}): {why}", host.name);
        }
        let dest = self.ship(alias)?;
        let out = self.runner.run(&cmd::apply(alias, &dest, &self.spec_file, &host.name, scope))?;
        if out.status != 0 {
            bail!("apply on {} exited {}", host.name, out.status);
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
