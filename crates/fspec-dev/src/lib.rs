//! Project development environments: a repo's `dev.star`, locked in the `fspec.lock` beside it,
//! activated by `fspec dev` or direnv's `use fspec`.

mod activate;
mod container;
pub mod gc;
pub mod libs;
mod python;
pub mod services;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use fspec_core::dev::Dev;
use fspec_core::resource::{Host, Resource};
use fspec_core::{Lock, Paths};
use sha2::{Digest, Sha256};

pub use activate::{Activated, Machine, activate, export, native_misses};
pub use container::Run;

pub const FILE: &str = "dev.star";

/// The shell function direnv's `use fspec` calls, for `~/.config/direnv/lib/fspec.sh`.
pub const DIRENV_LIB: &str = r#"# FormalSpec project environments: `use fspec` in an .envrc next to dev.star.
use_fspec() {
  watch_file dev.star fspec.lock
  local out
  out=$(fspec dev export --dir "$PWD") || return 1
  eval "$out"
}
"#;

/// A directory holding `dev.star`.
#[derive(Clone, Debug)]
pub struct Project {
    pub dir: PathBuf,
}

impl Project {
    /// The nearest directory at or above `from` that holds `dev.star`.
    pub fn find(from: &Path) -> Option<Project> {
        from.ancestors().find(|d| d.join(FILE).is_file()).map(|d| Project { dir: d.to_path_buf() })
    }

    pub fn file(&self) -> PathBuf {
        self.dir.join(FILE)
    }

    pub fn lock_path(&self) -> PathBuf {
        self.dir.join("fspec.lock")
    }

    pub fn evaluate(&self, platform: &str) -> Result<Dev> {
        let mut dev = fspec_eval::evaluate_dev(&self.file(), platform)?;
        if dev.packages.iter().any(python::is_pypi) && !dev.packages.iter().any(|p| p.name == "uv") {
            dev.packages.push(python::implicit_uv());
        }
        Ok(dev)
    }

    /// Eight hex digits naming this project in caches and container names.
    pub fn id(&self) -> String {
        short_hash(&self.dir.display().to_string())
    }

    /// The environment as each platform it locks for sees it.
    pub fn per_platform(&self, local: &str) -> Result<BTreeMap<String, Dev>> {
        let first = self.evaluate(local)?;
        let mut out = BTreeMap::new();
        for p in &first.platforms {
            let dev = if p == local { first.clone() } else { self.evaluate(p)? };
            if dev.platforms != first.platforms {
                bail!("{}: `platforms` must not depend on `platform`", self.file().display());
            }
            out.insert(p.clone(), dev);
        }
        Ok(out)
    }
}

pub(crate) fn short_hash(s: &str) -> String {
    Sha256::digest(s.as_bytes()).iter().take(4).map(|b| format!("{b:02x}")).collect()
}

pub struct Update {
    pub lock: Lock,
    pub changes: Vec<String>,
    pub errors: Vec<String>,
    /// `lib(...)` and `bin(...)` some package manager has no package for, with why.
    pub misses: Vec<String>,
}

/// `docker` or `podman`, whichever is installed, docker first.
pub fn runtime(runner: &dyn fspec_core::runner::Runner) -> Option<String> {
    let found = runner.run(&fspec_core::runner::Cmd::new("sh").args(["-c", "command -v docker || command -v podman"]));
    let path = found.ok().filter(|o| o.status == 0)?.stdout;
    let name = path.lines().next()?.rsplit('/').next()?.trim().to_string();
    (!name.is_empty()).then_some(name)
}

/// `fspec update` for a project: resolve every package for the platforms that declare it, and
/// pin each image the project runs (its container and its services) by digest.
pub fn update(project: &Project, only: &[String], cx: &fspec_sources::Cx, paths: &Paths) -> Result<Update> {
    let devs = project.per_platform(&cx.local.0)?;
    let hosts: Vec<Host> = devs
        .iter()
        .map(|(platform, dev)| Host {
            name: format!("{FILE} ({platform})"),
            platform: platform.clone(),
            system: None,
            users: vec![],
            resources: dev.packages.iter().filter(|p| !python::is_pypi(p)).cloned().map(Resource::Pkg).collect(),
            protect: vec![],
            persist: vec![],
            release_age: None,
            builders: vec![],
            sync: vec![],
            ssh: None,
            locale: None,
            keymap: None,
            timezone: None,
        })
        .collect();
    let old = Lock::load(&project.lock_path())?;
    let u = fspec_sources::update::update(&hosts, &old, only, cx)?;
    let pypi: BTreeSet<String> =
        devs.values().flat_map(|d| d.packages.iter().filter(|p| python::is_pypi(p)).map(|p| p.name.clone())).collect();
    let (mut lock, mut errors) = (u.lock, u.errors);
    // The sources update never sees pypi packages, so it reports them as removed.
    let mut changes: Vec<String> = u
        .changes
        .into_iter()
        .filter(|c| !c.strip_prefix("- ").is_some_and(|r| pypi.iter().any(|n| r.starts_with(&format!("{n} ")))))
        .collect();
    if let Err(e) = update_pypi(project, &devs, &old, &mut lock, &mut changes, only, cx, paths) {
        errors.push(format!("{e:#}"));
    }

    let images: BTreeSet<String> = devs
        .values()
        .flat_map(|d| {
            d.container
                .iter()
                .chain(&d.fallback_container)
                .cloned()
                .chain(d.services.iter().filter_map(|s| s.image_ref()))
        })
        .collect();
    lock.image.clear();
    for gone in old.image.keys().filter(|r| !images.contains(*r)) {
        changes.push(format!("- image {gone}"));
    }
    for r in &images {
        let before = old.image.get(r);
        if !only.is_empty() {
            if let Some(b) = before {
                lock.image.insert(r.clone(), b.clone());
            }
            continue;
        }
        match fspec_image::pin_image(cx.fetch, r) {
            Ok(pinned) => {
                match before {
                    None => changes.push(format!("+ image {r} {pinned}")),
                    Some(b) if *b != pinned => changes.push(format!("image {r} {b} -> {pinned}")),
                    Some(_) => {}
                }
                lock.image.insert(r.clone(), pinned);
            }
            Err(e) => {
                errors.push(format!("{e:#}"));
                if let Some(b) = before {
                    lock.image.insert(r.clone(), b.clone());
                }
            }
        }
    }
    let mut misses = Vec::new();
    if only.is_empty() {
        let r = libs::Resolver { runner: cx.runner, fetch: cx.fetch, runtime: runtime(cx.runner) };
        let resolved = libs::resolve(&devs, &old, &r)?;
        lock.native = resolved.native;
        changes.extend(resolved.changes);
        misses = resolved.misses;
        errors.extend(resolved.errors);
    }
    Ok(Update { lock, changes, errors, misses })
}

/// Compile each `pypi(...)` package with the locked uv, for the locked python's version, and
/// keep the old entry for any package `only` leaves out.
#[allow(clippy::too_many_arguments)]
fn update_pypi(
    project: &Project,
    devs: &BTreeMap<String, Dev>,
    old: &Lock,
    lock: &mut Lock,
    changes: &mut Vec<String>,
    only: &[String],
    cx: &fspec_sources::Cx,
    paths: &Paths,
) -> Result<()> {
    let mut wanted: BTreeMap<String, &fspec_core::resource::Pkg> = BTreeMap::new();
    let mut tools = None;
    for dev in devs.values() {
        for p in dev.packages.iter().filter(|p| python::is_pypi(p)) {
            wanted.insert(p.name.clone(), p);
        }
        if tools.is_none() {
            tools = python::tools(dev)?;
        }
    }
    let Some((python_pkg, uv_pkg)) = tools else { return Ok(()) };
    let python_version = lock
        .pkg(&python_pkg.name, &python_pkg.source.to_string())
        .and_then(|l| l.version.clone())
        .context("pkg python did not resolve, so pypi packages cannot be compiled")?;
    let local = &cx.local.0;
    let uv = activate::with_store(paths, cx.runner, cx.fetch, project, lock, local, |scx, shared, own| {
        let u = fspec_store::unpack_for_project(scx, uv_pkg, shared, own)?;
        let rel = u.bins.get("uv").context("the uv package has no uv command")?;
        Ok(u.dir.join(rel))
    })?;
    for (name, p) in wanted {
        let before = old.pkg(&name, &p.source.to_string());
        if !only.is_empty() && !only.contains(&name) {
            if let Some(b) = before {
                lock.upsert(b.clone());
            }
            continue;
        }
        let entry = python::compile(cx.runner, &uv, p, &python_version)?;
        let v = entry.version.clone().unwrap_or_default();
        match before.and_then(|b| b.version.clone()) {
            None => changes.push(format!("+ {name} {v}")),
            Some(was) if was != v => changes.push(format!("{name} {was} -> {v}")),
            Some(_) => {}
        }
        lock.upsert(entry);
    }
    Ok(())
}

/// Where the user's own copy of store packages goes when the system store lacks them.
pub fn user_store(home: &Path) -> PathBuf {
    match std::env::var_os("XDG_DATA_HOME") {
        Some(d) if !d.is_empty() => PathBuf::from(d).join("fspec/store"),
        _ => home.join(".local/share/fspec/store"),
    }
}

/// Per-project caches: link farms, container homes, service state.
pub fn cache_dir(home: &Path) -> PathBuf {
    match std::env::var_os("XDG_CACHE_HOME") {
        Some(d) if !d.is_empty() => PathBuf::from(d).join("fspec/dev"),
        _ => home.join(".cache/fspec/dev"),
    }
}
