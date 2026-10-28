//! The store backend: aqua, github, npm, cargo and nix packages unpack into
//! `<fspec root>/store/<hash8>-<name>-<version>/`, and each apply builds a generation of
//! symlinks into the store and switches `current` to it.

mod archive;
mod dev;
pub mod gc;
mod generation;
mod npm;
mod oci;
mod patch;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use fspec_core::apply::Records;
use fspec_core::facts::{Init, Os};
use fspec_core::journal::{Journal, copy_any, remove_any};
use fspec_core::lock::{Artifact, LockedPkg};
use fspec_core::par;
use fspec_core::resource::{Pkg, PkgMode, Source};
use fspec_core::runner::{Cmd, Runner};
use fspec_core::state::Entry;
use fspec_core::{Backend, Cx, Lock, Op, Phase, Resource, State};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use generation::{Generation, link_target, undo_create};

pub use archive::sri_sha256;
pub use dev::{Unpacked, unpack_for_project};

pub struct StoreBackend;

/// Pull a CI-built package by its locked OCI reference into `scratch` and unpack it into `dest`,
/// outside any apply: how a remote apply hands an older host the locked engine.
pub fn pull_built(runner: &dyn Runner, reference: &str, scratch: &Path, dest: &Path, name: &str) -> Result<()> {
    let body = oci::pull(runner, reference, scratch)?;
    archive::unpack(&body, dest, name)
}

/// What the state file keeps per package. `locked` is the lock entry as applied, which lets a
/// rollback rebuild the lock of an old generation.
#[derive(Serialize, Deserialize, Clone, Debug)]
struct Installed {
    /// Relative to the fspec root for our own store, absolute for `/nix/store` paths.
    store: String,
    version: String,
    /// The artifact hash, or the nixpkgs rev: what changed when the lock moved.
    id: String,
    /// Executable name to path inside `store`.
    bins: BTreeMap<String, String>,
    /// Path under `/usr/local/share` to the path inside `store` it links to.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    share: BTreeMap<String, String>,
    /// Path under `/etc` to the path inside `store` it links to.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    etc: BTreeMap<String, String>,
    /// App bundles inside `store`, each cloned to /Applications/<its file name>.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    apps: Vec<String>,
    locked: LockedPkg,
}

impl Installed {
    fn from_record(record: &Map<String, Value>) -> Option<Installed> {
        serde_json::from_value(Value::Object(record.clone())).ok()
    }

    fn to_record(&self) -> Map<String, Value> {
        match serde_json::to_value(self) {
            Ok(Value::Object(m)) => m,
            _ => Map::new(),
        }
    }

    fn exists(&self, root: &Path) -> bool {
        root.join(&self.store).exists()
    }
}

/// The lock of the generation whose state is `state`, rebuilt from what each apply recorded.
pub fn lock_from_state(state: &State) -> Lock {
    let mut lock = Lock::default();
    for entry in state.entries.values() {
        if let Some(inst) = Installed::from_record(&entry.record) {
            lock.upsert(inst.locked);
        }
    }
    lock
}

/// What the lock says to install for a package on this host.
struct Target<'a> {
    locked: &'a LockedPkg,
    version: String,
    id: String,
    kind: Kind<'a>,
}

enum Kind<'a> {
    Archive { origin: Origin<'a>, dirname: String, artifact: &'a Artifact },
    Nix { attr: &'a str },
}

/// Where an archive comes from. A CI build and a build here are already patched; a download is
/// patched here.
enum Origin<'a> {
    Url {
        url: &'a str,
        hash: &'a str,
    },
    Oci {
        reference: &'a str,
    },
    /// A `git(...)` package CI has no artifact for: built on the host's builders, else here.
    Build {
        url: &'a str,
        rev: &'a str,
    },
}

/// The artifact of a package built on a builder or here, which the lock has no entry for.
static BUILT: std::sync::LazyLock<Artifact> = std::sync::LazyLock::new(Artifact::default);

fn target<'a>(cx: &'a Cx, p: &'a Pkg) -> Result<Target<'a>> {
    let missing = || format!("pkg {} is not in fspec.lock, run fspec update", p.name);
    let locked = cx.lock.pkg(&p.name, &p.source.to_string()).with_context(missing)?;
    let version = locked.version.clone().or_else(|| locked.rev.clone()).unwrap_or_else(|| "unversioned".into());
    if let Source::Nix { attr } = &p.source {
        let id = locked.rev.clone().unwrap_or_else(|| version.clone());
        return Ok(Target { locked, version, id, kind: Kind::Nix { attr } });
    }
    fspec_core::patches::check(cx.spec_dir, &p.patches, locked.patches.as_deref())
        .with_context(|| format!("pkg {}", p.name))?;
    let platform = cx.facts.platform();
    let found = locked.artifacts.get(&platform).or_else(|| locked.artifacts.get("any"));
    if let Source::Git { url, .. } = &p.source {
        let rev = locked.rev.as_deref().with_context(|| format!("pkg {} has no rev in fspec.lock", p.name))?;
        // The recipe is part of what was built: a changed `build` or image builds anew. The cache
        // holds the same build, so a host that compiled it before CI pushed keeps its copy.
        let recipe = serde_json::to_string(&(&p.build, &p.build_flags, &p.source))?;
        let id = format!("{url}@{rev}+{}+{}", locked.patches.as_deref().unwrap_or_default(), archive::hash8(&recipe));
        let version = rev.chars().take(12).collect::<String>();
        let dirname = format!("{}-{}-{version}", archive::hash8(&id), p.name);
        let (origin, artifact) = match found.and_then(|a| a.oci.as_deref().map(|r| (r, a))) {
            Some((reference, a)) => (Origin::Oci { reference }, a),
            None => (Origin::Build { url, rev }, found.unwrap_or(&BUILT)),
        };
        return Ok(Target { locked, version, id, kind: Kind::Archive { origin, dirname, artifact } });
    }
    let artifact = found
        .with_context(|| format!("pkg {} has no artifact for {platform} in fspec.lock, run fspec update", p.name))?;
    if let Some(reference) = artifact.oci.as_deref() {
        let version = version.chars().take(12).collect::<String>();
        let dirname = format!("{}-{}-{version}", archive::hash8(reference), p.name);
        return Ok(Target {
            locked,
            version,
            id: reference.into(),
            kind: Kind::Archive { origin: Origin::Oci { reference }, dirname, artifact },
        });
    }
    let (Some(url), Some(hash)) = (artifact.url.as_deref(), artifact.hash.as_deref()) else {
        bail!(
            "pkg {} needs a url and a hash, or an oci reference, for {platform} in fspec.lock, run fspec update",
            p.name
        );
    };
    let mut identity = match &locked.patches {
        Some(patches) => format!("{hash}+{patches}"),
        None => hash.to_string(),
    };
    for (dest, part) in &p.parts {
        identity = format!("{identity}+{dest}={}", part_hash(p, locked, dest, part)?);
    }
    if !p.build.is_empty() {
        identity = format!("{identity}+{}", archive::hash8(&serde_json::to_string(&p.build)?));
    }
    let dirname = format!("{}-{}-{version}", archive::hash8(&identity), p.name);
    Ok(Target {
        locked,
        version,
        id: identity,
        kind: Kind::Archive { origin: Origin::Url { url, hash }, dirname, artifact },
    })
}

/// A self_managed package keeps whatever it has once installed.
fn keeps_installed(cx: &Cx, op: &Op, p: &Pkg) -> Option<Installed> {
    if p.mode != PkgMode::SelfManaged {
        return None;
    }
    let Entry { resource: Resource::Pkg(old), record, .. } = op.current.as_ref()? else { return None };
    let inst = Installed::from_record(record)?;
    (old.source == p.source && inst.exists(&cx.paths.fspec_root())).then_some(inst)
}

fn pkg_of(op: &Op) -> Result<&Pkg> {
    match op.resource() {
        Resource::Pkg(p) => Ok(p),
        other => bail!("the store backend got {}", other.key()),
    }
}

impl Backend for StoreBackend {
    fn name(&self) -> &'static str {
        "store"
    }

    fn handles(&self, _cx: &Cx, r: &Resource) -> bool {
        let Resource::Pkg(p) = r else { return false };
        matches!(
            p.source,
            Source::Aqua { .. }
                | Source::Github { .. }
                | Source::Npm { .. }
                | Source::Cargo { .. }
                | Source::Git { .. }
                | Source::Nix { .. }
                | Source::Url { .. }
                | Source::Python
        )
    }

    fn phase(&self) -> Phase {
        Phase::Store
    }

    fn check(&self, cx: &Cx, r: &Resource) -> Result<()> {
        let Resource::Pkg(p) = r else { return Ok(()) };
        if let Source::Git { deps, image, .. } = &p.source {
            match (image, cx.facts.os) {
                (Some(_), Os::Darwin) => {
                    bail!("git(image = ...) builds in a Linux container; on macOS deps install with brew")
                }
                (None, Os::Linux) if !deps.is_empty() => {
                    bail!("git(deps = ...) needs image = ... to install them into")
                }
                _ => {}
            }
        }
        if !p.build.is_empty() && !matches!(p.source, Source::Git { .. } | Source::Url { .. }) {
            bail!("build runs for git(...) and url(...) packages only");
        }
        if !p.share.is_empty() && cx.facts.os != Os::Linux {
            bail!("share links into /usr/local/share, which only Linux searches");
        }
        if !p.apps.is_empty() && cx.facts.os != Os::Darwin {
            bail!("apps go in /Applications, which only macOS has");
        }
        let normal = |s: &str| Path::new(s).components().all(|c| matches!(c, Component::Normal(_)));
        for app in &p.apps {
            if !normal(app) || !app.ends_with(".app") {
                bail!("apps: `{app}` must be the path of a .app bundle inside the package");
            }
        }
        for (dest, rel) in &p.share {
            if !normal(dest) || Path::new(dest).components().count() < 2 {
                bail!("share: `{dest}` must name an entry inside a directory of /usr/local/share, like icons/<theme>");
            }
            if rel != "." && !normal(rel) {
                bail!("share: `{rel}` must be a path inside the package, or `.`");
            }
        }
        if !p.etc.is_empty() && cx.facts.os != Os::Linux {
            bail!("etc links into /etc for Linux services; on macOS use file(..., target = ...)");
        }
        for (dest, rel) in &p.etc {
            if !normal(dest) || !normal(rel) {
                bail!("etc: `{dest}` and `{rel}` must be paths under /etc and inside the package");
            }
        }
        if !p.parts.is_empty() && matches!(p.source, Source::Git { .. } | Source::Nix { .. }) {
            bail!("parts unpack beside a downloaded archive; a {} package has none", p.source.kind());
        }
        for (dest, part) in &p.parts {
            if !normal(dest) || !matches!(part, Source::Url { .. }) {
                bail!("parts: `{dest}` must be a path inside the package and take a url(...) source");
            }
        }
        Ok(())
    }

    /// The lock moved past the installed version (pinned only), or a link went missing.
    fn drift(&self, cx: &Cx, entry: &Entry) -> Result<Vec<String>> {
        let Resource::Pkg(p) = &entry.resource else { return Ok(vec![]) };
        let root = cx.paths.fspec_root();
        let Some(inst) = Installed::from_record(&entry.record) else { return Ok(vec!["no install recorded".into()]) };
        let mut notes = Vec::new();
        if p.mode == PkgMode::Pinned {
            let locked = cx.lock.pkg(&p.name, &p.source.to_string()).and_then(|l| l.patches.as_deref());
            if let Err(e) = fspec_core::patches::check(cx.spec_dir, &p.patches, locked) {
                notes.push(e.to_string());
            } else if let Ok(t) = target(cx, p)
                && t.id != inst.id
            {
                notes.push(format!("locked {} (installed {})", t.version, inst.version));
            }
        }
        if !inst.exists(&root) {
            notes.push(format!("{} is missing from the store", inst.store));
        }
        let broken: Vec<_> =
            inst.bins.keys().filter(|b| fs::metadata(root.join("current/bin").join(b)).is_err()).cloned().collect();
        if !broken.is_empty() {
            notes.push(format!("not linked in current: {}", broken.join(", ")));
        }
        for (base, links) in [(SHARE, &inst.share), (ETC, &inst.etc)] {
            let missing: Vec<_> = links.keys().filter(|d| !is_link(&under(cx, base, d))).cloned().collect();
            if !missing.is_empty() {
                notes.push(format!("not linked in {base}: {}", missing.join(", ")));
            }
        }
        let missing: Vec<_> = inst.apps.iter().map(|a| app_name(a)).filter(|n| !app_path(cx, n).is_dir()).collect();
        if !missing.is_empty() {
            notes.push(format!("not in /Applications: {}", missing.join(", ")));
        }
        Ok(notes)
    }

    /// Downloads and registry pulls wait on the network, so they run at once, each into its own
    /// staging directory. Builds run after them, one at a time: each takes the whole machine
    /// and shares the build cache.
    fn prepare(&self, cx: &Cx, ops: &[Op], staging: &Path) -> Result<()> {
        let root = cx.paths.fspec_root();
        let mut archives = Vec::new();
        for op in ops.iter().filter(|o| !o.is_removal()) {
            let p = pkg_of(op)?;
            if keeps_installed(cx, op, p).is_some() {
                continue;
            }
            let Target { locked, kind, .. } = target(cx, p)?;
            match kind {
                Kind::Nix { attr } => {
                    let out = nix_build(cx, locked, attr).with_context(|| format!("building {}", p.name))?;
                    let marker = staging.join("nix").join(&p.name);
                    fs::create_dir_all(marker.parent().context("marker has a parent")?)?;
                    fs::write(&marker, out)?;
                }
                Kind::Archive { origin, dirname, artifact } => {
                    if root.join("store").join(&dirname).exists() || !build_tool_present(p, &origin) {
                        continue;
                    }
                    archives.push((p, origin, dirname, artifact));
                }
            }
        }
        let (builds, fetches): (Vec<_>, Vec<_>) =
            archives.into_iter().partition(|(_, origin, _, _)| matches!(origin, Origin::Build { .. }));
        let stage = |(p, origin, dirname, artifact): &(&Pkg, Origin, String, &Artifact)| -> Result<()> {
            let staged = staging.join("store").join(dirname);
            unpack_into(cx, p, origin, dirname, &staged, staging)?;
            // A wrong `bins`, `links` or `share` fails here, before an earlier phase has
            // installed the distro's packages that a rollback would then remove again.
            bins_of(p, artifact, &staged)
                .and_then(|bins| check_tree(p, &bins, &staged))
                .with_context(|| format!("checking {}", p.name))
        };
        par::try_map(&fetches, stage)?;
        builds.iter().try_for_each(stage)
    }

    fn commit(&self, cx: &Cx, ops: &[Op], staging: &Path, journal: &mut Journal) -> Result<Records> {
        let root = cx.paths.fspec_root();
        fs::create_dir_all(root.join("store")).with_context(|| format!("creating {}", root.display()))?;
        let generation = Generation::open(&root, cx.generation, journal)?;
        let mut records = Records::new();
        let mut reload_units = false;
        let mut reload_bus = false;
        for op in ops {
            let p = pkg_of(op)?;
            let before = op.current.as_ref().and_then(|e| Installed::from_record(&e.record));
            if let Some(before) = &before {
                for name in before.bins.keys() {
                    generation.unlink(name)?;
                }
                for dest in before.share.keys() {
                    unshare(cx, SHARE, dest, journal)?;
                }
                for dest in before.etc.keys() {
                    unshare(cx, ETC, dest, journal)?;
                    reload_units |= dest.starts_with("systemd/");
                    reload_bus |= dest.starts_with("dbus-1/");
                }
                for app in &before.apps {
                    let at = app_path(cx, app_name(app));
                    journal.backup(&at)?;
                    remove_any(&at)?;
                }
                generation.unlink_pkg(&p.name)?;
            }
            if op.is_removal() {
                continue;
            }
            let inst = match keeps_installed(cx, op, p) {
                // A self-managed package keeps its tree, not the apps an older spec listed.
                Some(kept) => Installed { apps: p.apps.clone(), ..kept },
                None => install(cx, p, staging, journal).with_context(|| format!("installing {}", p.name))?,
            };
            for (name, rel) in &inst.bins {
                generation.link(name, &link_target(&inst.store, rel))?;
            }
            generation.link_pkg(&p.name, &link_target(&inst.store, ""))?;
            for (dest, rel) in &inst.share {
                share(cx, SHARE, dest, &live_path(&inst.store, rel), journal)?;
            }
            for (dest, rel) in &inst.etc {
                share(cx, ETC, dest, &live_path(&inst.store, rel), journal)?;
            }
            reload_units |= inst.etc.keys().any(|d| d.starts_with("systemd/"));
            reload_bus |= inst.etc.keys().any(|d| d.starts_with("dbus-1/"));
            for app in &inst.apps {
                place_app(cx, &root, &inst.store, app, journal)?;
            }
            records.insert(op.key.clone(), inst.to_record());
        }
        generation.switch(journal)?;
        // A unit linked under /etc/systemd is enabled by the services phase, after this one.
        if reload_units && cx.facts.init == Init::Systemd {
            let reload = Cmd::new("systemctl").arg("daemon-reload");
            journal.record(fspec_core::journal::Undo::Run { cmd: reload.clone() })?;
            cx.runner.check(&reload)?;
        }
        // dbus-broker does not watch its policy directories, so a service the services phase starts
        // would be refused the bus name its new policy grants.
        if reload_bus && cx.facts.init == Init::Systemd {
            let reload = Cmd::new("systemctl").args(["reload", "dbus.service"]);
            journal.record(fspec_core::journal::Undo::Run { cmd: reload.clone() })?;
            cx.runner.check(&reload)?;
        }
        Ok(records)
    }
}

/// Download (or pull) a package's locked artifact, verify it and unpack it into `dest`, patched
/// and, for npm, installed. `scratch` holds downloads on the way.
/// Whether the tool `unpack_into` runs for `p` is on PATH. When it is not, the build waits for
/// commit, which runs after the Packages phase that may install it.
fn build_tool_present(p: &Pkg, origin: &Origin) -> bool {
    let tool = match (origin, &p.source) {
        (Origin::Oci { .. }, _) => "oras",
        (_, Source::Npm { .. }) => "npm",
        _ => return true,
    };
    std::env::var_os("PATH").is_some_and(|path| std::env::split_paths(&path).any(|d| d.join(tool).is_file()))
}

/// A disk image mounts through hdiutil; everything else unpacks in process.
fn unpack(cx: &Cx, body: &[u8], dest: &Path, name: &str) -> Result<()> {
    if archive::is_dmg(body) {
        return archive::undmg(cx.runner, body, dest);
    }
    archive::unpack(body, dest, name)
}

fn unpack_into(cx: &Cx, p: &Pkg, origin: &Origin, dirname: &str, dest: &Path, scratch: &Path) -> Result<()> {
    match origin {
        Origin::Build { url, rev } => {
            let (platform, out) = (cx.facts.platform(), scratch.join("build").join(dirname));
            let cache = match cx.build_cache {
                Some(c) => c.to_path_buf(),
                None => fspec_build::cache_root()?,
            };
            let job = fspec_build::Job {
                pkg: p,
                url,
                rev,
                platform: &platform,
                spec_dir: cx.spec_dir,
                out: &out,
                cache: &cache,
            };
            let built = fspec_build::builders::build_anywhere(cx.runner, cx.builders, &job)
                .with_context(|| format!("building {}", p.name))?;
            let body = fs::read(&built)?;
            unpack(cx, &body, dest, &p.name).with_context(|| format!("unpacking {}", built.display()))?;
        }
        Origin::Oci { reference } => {
            let body = oci::pull(cx.runner, reference, &scratch.join("oci").join(dirname))
                .with_context(|| format!("pulling {reference}"))?;
            unpack(cx, &body, dest, &p.name).with_context(|| format!("unpacking {reference}"))?;
        }
        Origin::Url { url, hash } => {
            let body = cx.fetch.get(url)?;
            archive::verify(&body, hash).with_context(|| format!("verifying {url}"))?;
            match &p.source {
                // A file that is neither an archive nor an ELF executable (an AppImage is one)
                // keeps its own name and is data, not a command.
                Source::Url { .. } if !archive::is_archive(&body) && !archive::is_elf(&body) => {
                    let name = url.rsplit('/').next().filter(|n| !n.is_empty()).unwrap_or(&p.name);
                    fs::create_dir_all(dest)?;
                    fs::write(dest.join(name), &body)?;
                }
                _ => unpack(cx, &body, dest, &p.name).with_context(|| format!("unpacking {url}"))?,
            }
            unpack_parts(cx, p, dest)?;
            if let Source::Npm { name } = &p.source {
                let tarball = scratch.join("dl").join(format!("{dirname}.tgz"));
                fs::create_dir_all(tarball.parent().context("tarball has a parent")?)?;
                fs::write(&tarball, &body)?;
                npm::finish(cx.runner, dest, &tarball, name)?;
            }
            if !p.patches.is_empty() {
                let tree = patch_root(dest, &p.source);
                patch::apply_all(&tree, cx.spec_dir, &p.patches).with_context(|| format!("patching {}", p.name))?;
            }
            if !p.build.is_empty() {
                build_download(cx, p, dest, &scratch.join("src").join(dirname))?;
            }
        }
    }
    Ok(())
}

/// A url(...) package's `build`: the command reads the unpacked download from `FSPEC_SRC` and
/// installs the package into `FSPEC_OUT`. It runs on the host, at each install.
fn build_download(cx: &Cx, p: &Pkg, dest: &Path, src: &Path) -> Result<()> {
    if src.exists() {
        fs::remove_dir_all(src)?;
    }
    fs::create_dir_all(src.parent().context("the source dir has a parent")?)?;
    fs::rename(dest, src)?;
    fs::create_dir_all(dest)?;
    let (program, args) = p.build.split_first().context("build has a command")?;
    let cmd = Cmd::new(program)
        .args(args)
        .env("FSPEC_SRC", src.display().to_string())
        .env("FSPEC_OUT", dest.display().to_string());
    let built = cx.runner.check(&cmd).with_context(|| format!("building {}", p.name));
    fs::remove_dir_all(src)?;
    built.map(drop)
}

/// Each of `p.parts` at its locked url and hash, unpacked into its path under the package root.
fn unpack_parts(cx: &Cx, p: &Pkg, dest: &Path) -> Result<()> {
    if p.parts.is_empty() {
        return Ok(());
    }
    let root = patch_root(dest, &p.source);
    let locked = cx.lock.pkg(&p.name, &p.source.to_string()).context("the package is not in fspec.lock")?;
    for (rel, part) in &p.parts {
        let hash = part_hash(p, locked, rel, part)?;
        let Source::Url { url } = part else { unreachable!("part_hash takes only url parts") };
        let body = cx.fetch.get(url)?;
        archive::verify(&body, hash).with_context(|| format!("verifying {url}"))?;
        unpack(cx, &body, &root.join(rel), &p.name).with_context(|| format!("unpacking {url} into {rel}"))?;
    }
    Ok(())
}

/// The locked hash of the part at `dest`, when the lock has it for the url the spec names.
fn part_hash<'l>(p: &Pkg, locked: &'l LockedPkg, dest: &str, part: &Source) -> Result<&'l str> {
    let Source::Url { url } = part else { bail!("pkg {}: parts take url(...) sources", p.name) };
    locked
        .parts
        .get(dest)
        .filter(|a| a.url.as_deref() == Some(url.as_str()))
        .and_then(|a| a.hash.as_deref())
        .with_context(|| format!("pkg {} has no hash for {url} in fspec.lock, run fspec update", p.name))
}

/// Where patch paths are relative to: the package directory inside an npm install or the single
/// top-level directory of an archive, else the unpacked root.
fn patch_root(dest: &Path, source: &Source) -> PathBuf {
    if let Source::Npm { name } = source {
        let installed = dest.join("node_modules").join(name);
        return if installed.is_dir() { installed } else { dest.join("package") };
    }
    let mut entries = fs::read_dir(dest).into_iter().flatten().flatten();
    match (entries.next(), entries.next()) {
        (Some(only), None) if only.path().is_dir() => only.path(),
        _ => dest.to_path_buf(),
    }
}

#[derive(Serialize, Deserialize)]
struct Sidecar {
    name: String,
    version: String,
    bins: BTreeMap<String, String>,
}

/// Put the package's tree in the store (unless it is already there) and work out its executables.
fn install(cx: &Cx, p: &Pkg, staging: &Path, journal: &mut Journal) -> Result<Installed> {
    let root = cx.paths.fspec_root();
    let t = target(cx, p)?;
    let (store, bins) = match t.kind {
        Kind::Nix { .. } => {
            let out = fs::read_to_string(staging.join("nix").join(&p.name)).context("nix output was not prepared")?;
            let bins = nix_bins(Path::new(&out))?;
            (out, bins)
        }
        Kind::Archive { dirname, artifact, .. } => {
            let live = root.join("store").join(&dirname);
            if !live.exists() {
                let sidecar = root.join("store").join(format!("{dirname}.json"));
                undo_create(journal, &live)?;
                undo_create(journal, &sidecar)?;
                let from = staging.join("store").join(&dirname);
                if !from.exists() {
                    // Prepare skipped it: its build tool was not installed yet.
                    let Kind::Archive { origin, .. } = &target(cx, p)?.kind else { unreachable!() };
                    unpack_into(cx, p, origin, &dirname, &from, staging)?;
                }
                fs::rename(&from, &live).or_else(|_| copy_any(&from, &live).and_then(|()| remove_any(&from)))?;
                let meta =
                    Sidecar { name: p.name.clone(), version: t.version.clone(), bins: bins_of(p, artifact, &live)? };
                fs::write(&sidecar, serde_json::to_vec_pretty(&meta)?)?;
            }
            let bins = bins_of(p, artifact, &live)?;
            (format!("store/{dirname}"), bins)
        }
    };
    let tree = if Path::new(&store).is_absolute() { PathBuf::from(&store) } else { root.join(&store) };
    check_tree(p, &bins, &tree)?;
    Ok(Installed {
        store,
        version: t.version,
        id: t.id,
        bins,
        share: p.share.clone(),
        etc: p.etc.clone(),
        apps: p.apps.clone(),
        locked: t.locked.clone(),
    })
}

/// What the spec names in the package's tree is there.
fn check_tree(p: &Pkg, bins: &BTreeMap<String, String>, tree: &Path) -> Result<()> {
    // A source build may install only a library or data (a PAM module), which current/pkgs/<name>
    // reaches; a download with nothing to link is a wrong artifact.
    if bins.is_empty() && p.share.is_empty() && p.apps.is_empty() && !matches!(p.source, Source::Git { .. }) {
        bail!("it has no executables to link, no share and no apps");
    }
    for (field, links) in [("share", &p.share), ("etc", &p.etc)] {
        for (dest, rel) in links {
            if fs::symlink_metadata(tree.join(rel)).is_err() {
                bail!("{field}: `{rel}` (for {dest}) is not in the package");
            }
        }
    }
    for app in &p.apps {
        if !tree.join(app).is_dir() {
            bail!("apps: `{app}` is not in the package");
        }
    }
    Ok(())
}

fn is_link(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

const SHARE: &str = "/usr/local/share";
const ETC: &str = "/etc";

fn under(cx: &Cx, base: &str, dest: &str) -> PathBuf {
    cx.paths.system(base).join(dest)
}

/// What a share link points at on the live system: our store is always `/opt/fspec/store`,
/// whatever root this apply writes under.
fn live_path(store: &str, rel: &str) -> PathBuf {
    let base = if Path::new(store).is_absolute() { PathBuf::from(store) } else { Path::new("/opt/fspec").join(store) };
    if rel == "." { base } else { base.join(rel) }
}

/// Link `<base>/<dest>` to `target`, refusing to replace anything but a link.
fn share(cx: &Cx, base: &str, dest: &str, target: &Path, journal: &mut Journal) -> Result<()> {
    let at = under(cx, base, dest);
    if fs::symlink_metadata(&at).is_ok_and(|m| !m.file_type().is_symlink()) {
        bail!("{} exists and is not a link fspec made", at.display());
    }
    journal.backup(&at)?;
    remove_any(&at)?;
    fs::create_dir_all(at.parent().context("a share path has a parent")?)?;
    std::os::unix::fs::symlink(target, &at).with_context(|| format!("linking {}", at.display()))
}

fn app_name(app: &str) -> &str {
    app.rsplit('/').next().unwrap_or(app)
}

fn app_path(cx: &Cx, name: &str) -> PathBuf {
    cx.paths.system("/Applications").join(name)
}

/// Clone the bundle into /Applications. Spotlight, Launchpad and the Dock skip a symlink into
/// the store, so the bundle is a copy; on APFS a clone shares the store's blocks. The commit has
/// already removed the copy the previous install placed, so anything still there is not ours.
fn place_app(cx: &Cx, root: &Path, store: &str, app: &str, journal: &mut Journal) -> Result<()> {
    let at = app_path(cx, app_name(app));
    if fs::symlink_metadata(&at).is_ok() {
        bail!("{} exists and fspec did not put it there", at.display());
    }
    let from = if Path::new(store).is_absolute() { PathBuf::from(store) } else { root.join(store) }.join(app);
    journal.backup(&at)?;
    fs::create_dir_all(at.parent().context("an app path has a parent")?)?;
    clone_tree(&from, &at).with_context(|| format!("copying {} to {}", from.display(), at.display()))
}

#[cfg(target_os = "macos")]
fn clone_tree(from: &Path, to: &Path) -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let (src, dst) = (CString::new(from.as_os_str().as_bytes())?, CString::new(to.as_os_str().as_bytes())?);
    // SAFETY: both are valid NUL-terminated paths for the duration of the call.
    if unsafe { libc::clonefile(src.as_ptr(), dst.as_ptr(), 0) } == 0 {
        return Ok(());
    }
    // Another volume, or a filesystem without clones.
    copy_any(from, to)
}

#[cfg(not(target_os = "macos"))]
fn clone_tree(from: &Path, to: &Path) -> Result<()> {
    copy_any(from, to)
}

fn unshare(cx: &Cx, base: &str, dest: &str, journal: &mut Journal) -> Result<()> {
    let at = under(cx, base, dest);
    if is_link(&at) {
        journal.backup(&at)?;
        remove_any(&at)?;
    }
    Ok(())
}

fn bins_of(p: &Pkg, artifact: &Artifact, dir: &Path) -> Result<BTreeMap<String, String>> {
    let mut bins = if let Source::Npm { name } = &p.source {
        npm::bins(dir, name)?
    } else if artifact.bins.is_empty() {
        discover(dir)
    } else {
        let mut bins = BTreeMap::new();
        for entry in &artifact.bins {
            let (name, rel) = match entry.split_once('=') {
                Some((name, rel)) => (name, rel),
                None => (entry.rsplit('/').next().unwrap_or(entry), entry.as_str()),
            };
            if !dir.join(rel).is_file() {
                bail!("bin `{rel}` from fspec.lock is not in the artifact");
            }
            bins.insert(name.to_string(), rel.to_string());
        }
        bins
    };
    if !p.bins.is_empty() {
        let missing: Vec<&String> = p.bins.iter().filter(|b| !bins.contains_key(*b)).collect();
        if !missing.is_empty() {
            bail!(
                "bins: {} not among its executables ({})",
                missing.iter().map(|b| format!("`{b}`")).collect::<Vec<_>>().join(", "),
                bins.keys().cloned().collect::<Vec<_>>().join(", ")
            );
        }
        bins.retain(|name, _| p.bins.contains(name));
    }
    for (name, target) in &p.links {
        let rel = bins.get(target).cloned().with_context(|| {
            format!(
                "links: `{target}` is not one of its executables ({})",
                bins.keys().cloned().collect::<Vec<_>>().join(", ")
            )
        })?;
        bins.insert(name.clone(), rel);
    }
    Ok(bins)
}

/// The executables of a tree the lock names none for. A tree laid out as an install prefix, with
/// `bin/` at its root, in its single top-level directory or under `usr/` (a distro package such as
/// an Arch `.pkg.tar.zst`, whose root also holds `etc/` and metadata), links what that `bin/` holds, so
/// helpers under `lib/` or `libexec/` stay private. Any other tree links every executable regular
/// file in its top two levels, shallower ones winning name clashes.
fn discover(dir: &Path) -> BTreeMap<String, String> {
    if let Some((bin, prefix)) = prefix_bin(dir) {
        let mut bins = BTreeMap::new();
        for entry in fs::read_dir(&bin).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            if fs::metadata(entry.path()).is_ok_and(|m| m.is_file() && is_executable(&m)) {
                bins.insert(name.clone(), format!("{prefix}bin/{name}"));
            }
        }
        return bins;
    }
    let mut bins = BTreeMap::new();
    let mut level = vec![(dir.to_path_buf(), String::new())];
    for depth in 0..2 {
        let mut subdirs = Vec::new();
        for (path, prefix) in &level {
            let mut entries: Vec<_> = fs::read_dir(path).into_iter().flatten().flatten().collect();
            entries.sort_by_key(|e| e.file_name());
            for entry in entries {
                let name = entry.file_name().to_string_lossy().into_owned();
                let Ok(meta) = fs::symlink_metadata(entry.path()) else { continue };
                if meta.is_dir() && depth == 0 {
                    subdirs.push((entry.path(), format!("{prefix}{name}/")));
                } else if meta.is_file() && is_executable(&meta) && !name.starts_with('.') {
                    bins.entry(name.clone()).or_insert(format!("{prefix}{name}"));
                }
            }
        }
        level = subdirs;
    }
    bins
}

/// `bin/` at the root of the tree or inside its only entry, with the path prefix that leads to it.
fn prefix_bin(dir: &Path) -> Option<(PathBuf, String)> {
    let is_dir = |p: &Path| fs::symlink_metadata(p).is_ok_and(|m| m.is_dir());
    if is_dir(&dir.join("bin")) {
        return Some((dir.join("bin"), String::new()));
    }
    if is_dir(&dir.join("usr/bin")) {
        return Some((dir.join("usr/bin"), "usr/".into()));
    }
    let mut entries = fs::read_dir(dir).ok()?.flatten();
    let (Some(only), None) = (entries.next(), entries.next()) else { return None };
    let bin = only.path().join("bin");
    is_dir(&bin).then(|| (bin, format!("{}/", only.file_name().to_string_lossy())))
}

fn is_executable(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

fn nix_build(cx: &Cx, locked: &LockedPkg, attr: &str) -> Result<String> {
    let flake = match &locked.rev {
        Some(rev) => format!("github:NixOS/nixpkgs/{rev}#{attr}"),
        None => format!("nixpkgs#{attr}"),
    };
    let out = cx.runner.check(&Cmd::new("nix").args(["build", "--no-link", "--print-out-paths", &flake]))?;
    let path = out.stdout.lines().next().unwrap_or("").trim();
    if !path.starts_with('/') {
        bail!("`nix build {flake}` printed no store path");
    }
    Ok(path.into())
}

fn nix_bins(out: &Path) -> Result<BTreeMap<String, String>> {
    let mut bins = BTreeMap::new();
    let dir = out.join("bin");
    for entry in fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        bins.insert(name.clone(), format!("bin/{name}"));
    }
    Ok(bins)
}
