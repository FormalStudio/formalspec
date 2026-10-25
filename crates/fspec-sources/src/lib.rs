//! Turns a package declaration into a [`LockedPkg`]: exact version, URL and hash per platform.
//! Resolvers are the only code that talks to registries; apply installs from the lock alone.

mod age;
mod aqua;
mod cargo;
mod distro;
mod git;
mod github;
mod hash;
mod npm;
mod python;
mod service;
mod snapshot;
pub mod update;

use std::path::PathBuf;
use std::sync::OnceLock;

use anyhow::{Result, anyhow};
use fspec_core::facts::Facts;
use fspec_core::fetch::Fetch;
use fspec_core::lock::{Artifact, LockedPkg};
use fspec_core::resource::{Channel, Pkg, Source};
use fspec_core::runner::Runner;

pub use age::parse_age;

/// What resolvers read the world through.
pub struct Cx<'a> {
    pub fetch: &'a dyn Fetch,
    pub runner: &'a dyn Runner,
    /// Platform and distro of the machine running the update; only it can ask a package manager.
    pub local: (String, String),
    /// Unix seconds, the reference point for release-age cutoffs.
    pub now: i64,
    /// The directory holding `spec.star`, where a package's patch files are read for the lock's
    /// `patches` hash.
    pub spec_dir: PathBuf,
    pub(crate) registry: OnceLock<aqua::Registry>,
}

impl<'a> Cx<'a> {
    pub fn new(fetch: &'a dyn Fetch, runner: &'a dyn Runner) -> Cx<'a> {
        Cx {
            fetch,
            runner,
            local: Facts::probe_local(),
            now: age::now(),
            spec_dir: PathBuf::new(),
            registry: OnceLock::new(),
        }
    }

    pub fn with_spec_dir(mut self, dir: impl Into<PathBuf>) -> Cx<'a> {
        self.spec_dir = dir.into();
        self
    }
}

/// Resolve `pkg` for every platform in `platforms` (`x86_64-linux`, ...). `min_age` is the
/// longest release-age delay among the hosts declaring the package; a bleeding package ignores it.
pub fn resolve(pkg: &Pkg, platforms: &[String], min_age: Option<i64>, cx: &Cx) -> Result<LockedPkg> {
    let version = pkg.version.as_deref();
    let min_age = if pkg.channel == Channel::Bleeding || version.is_some() { None } else { min_age };
    let cutoff = min_age.map(|a| cx.now - a);
    let mut locked = LockedPkg {
        name: pkg.name.clone(),
        source: pkg.source.to_string(),
        version: None,
        rev: None,
        patches: None,
        parts: Default::default(),
        artifacts: Default::default(),
    };
    let ctx = || format!("resolving {} ({})", pkg.name, pkg.source);
    let pinnable = matches!(
        pkg.source,
        Source::Aqua { .. }
            | Source::Github { .. }
            | Source::Npm { .. }
            | Source::Cargo { .. }
            | Source::Python
            | Source::Pypi { .. }
    );
    if version.is_some() && !pinnable {
        return Err(anyhow!(
            "{}: version = is supported for aqua, github, npm, cargo, python and pypi packages",
            ctx()
        ));
    }
    let result = match &pkg.source {
        Source::Aqua { name } => aqua::resolve(name, &pkg.name, version, platforms, cutoff, cx, &mut locked),
        Source::Github { repo, asset } => {
            github::resolve(repo, asset.as_deref(), version, platforms, cutoff, cx, &mut locked)
        }
        Source::Npm { name } => npm::resolve(name, version, cutoff, cx, &mut locked),
        Source::Cargo { name } => cargo::resolve(name, version, platforms, cutoff, cx, &mut locked),
        Source::Flatpak { id, remote } => {
            service::flatpak(id.as_deref().unwrap_or(&pkg.name), remote, platforms, cx, &mut locked)
        }
        // formulae.brew.sh knows only Homebrew's own taps; a third-party one installs what it has.
        Source::Brew { tap: Some(_), .. } => Ok(()),
        Source::Brew { name, cask, .. } => service::brew(name.as_deref().unwrap_or(&pkg.name), *cask, cx, &mut locked),
        Source::Git { url, rev, paths, .. } => git::resolve(url, rev, paths, cx, &mut locked),
        Source::Nix { attr } => service::nix(attr, cx, &mut locked),
        Source::Dnf(_) | Source::Pacman(_) | Source::Apt(_) | Source::Xbps(_) => {
            distro::resolve(&pkg.source, &pkg.name, platforms, cx, &mut locked)
        }
        // moss installs from a repository stream, not from package files, and rolls back through
        // its own states, so its packages lock by name only. zypper packages do too: the
        // host's pinned Tumbleweed snapshot fixes their versions.
        Source::Moss(_) | Source::Zypper(_) => Ok(()),
        Source::Python => python::resolve(version, platforms, cx, &mut locked),
        Source::Url { url } => python::url(url, cx, &mut locked),
        Source::Pypi { .. } => Err(anyhow!("pypi(...) packages belong in a dev.star, which fspec dev locks")),
    };
    result.map_err(|e| anyhow!("{}: {e:#}", ctx()))?;
    for (dest, part) in &pkg.parts {
        let Source::Url { url } = part else { return Err(anyhow!("{}: parts take url(...) sources", ctx())) };
        let hash = hash::sri(&cx.fetch.get(url).map_err(|e| anyhow!("{}: part {dest}: {e:#}", ctx()))?);
        locked.parts.insert(dest.clone(), Artifact { url: Some(url.clone()), hash: Some(hash), ..Default::default() });
    }
    if !pkg.patches.is_empty() {
        locked.patches =
            Some(fspec_core::patches::hash(&cx.spec_dir, &pkg.patches).map_err(|e| anyhow!("{}: {e:#}", ctx()))?);
    }
    Ok(locked)
}
