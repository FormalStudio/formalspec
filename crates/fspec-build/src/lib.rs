//! Source builds for `git(...)` packages and the CI cache that keeps hosts from compiling:
//! `fspec build` makes one `.tar.zst` per package and platform, the generated workflow pushes it
//! to an OCI registry, and `fspec update --cache` records the digests in the lock.

pub mod builders;
pub mod cache;
pub mod ci;
mod pack;
pub mod recipe;
mod run;

pub use run::{Built, Job, build, cache_root};
