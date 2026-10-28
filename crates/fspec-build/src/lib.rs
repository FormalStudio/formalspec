//! Source builds for `git(...)` packages and the CI cache that keeps hosts from compiling:
//! `fspec build` makes one `.tar.zst` per package and platform, the generated workflow pushes it
//! to an OCI registry, and `fspec update --cache` records the digests in the lock.

pub mod builders;
pub mod cache;
pub mod ci;
mod pack;
pub mod recipe;
mod run;

pub use run::{Built, Job, build, cache_root, job_tag};

use anyhow::Result;
use fspec_core::resource::Pkg;
use sha2::{Digest, Sha256};

/// Everything a `git(...)` build is made of: the commit, the locked patches hash and the recipe
/// (`build`, `build_flags`, the source with its image and deps). Two builds with the same
/// identity are the same build, wherever they ran.
pub fn identity(p: &Pkg, url: &str, rev: &str, patches: Option<&str>) -> Result<String> {
    let recipe = serde_json::to_string(&(&p.build, &p.build_flags, &p.source))?;
    Ok(format!("{url}@{rev}+{}+{}", patches.unwrap_or_default(), hash8(&recipe)))
}

/// The build key of an identity, which the cache tag and the lock's `build` carry.
pub fn key(identity: &str) -> String {
    hash8(identity)
}

/// The registry tag of a build: `<rev>-<key>-<platform>`.
pub fn tag(rev: &str, identity: &str, platform: &str) -> String {
    format!("{rev}-{}-{platform}", key(identity))
}

fn hash8(s: &str) -> String {
    Sha256::digest(s.as_bytes()).iter().take(4).map(|b| format!("{b:02x}")).collect()
}
