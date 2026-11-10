//! Package manager backends: Flatpak and Homebrew everywhere they exist, dnf, pacman, apt, xbps,
//! moss and zypper on their classic distros, the base packages and repos of those distros, and
//! Mac apps through atoll.
//! Each one journals the inverse command before it changes anything.

mod atoll;
mod brew;
mod distro;
mod family;
mod flatpak;
mod managers;
mod pkgs;
#[cfg(test)]
mod tests;
mod zypper;

use anyhow::{Result, bail};
use fspec_core::Cx;
use fspec_core::lock::LockedPkg;
use fspec_core::resource::{Pkg, Resource};
use fspec_core::runner::{Cmd, Output};
use serde_json::{Map, Value};

pub use atoll::AtollBackend;
pub use brew::BrewBackend;
pub use distro::DistroBackend;
pub use flatpak::FlatpakBackend;
pub use managers::{AptBackend, DnfBackend, MossBackend, PacmanBackend, XbpsBackend, ZypperBackend};

fn pkg(r: &Resource) -> Result<&Pkg> {
    match r {
        Resource::Pkg(p) => Ok(p),
        other => bail!("{} is not a package", other.key()),
    }
}

/// The lock entry for `p`. Fails when the lock was written for a different source, which
/// `fspec update` fixes.
fn locked<'a>(cx: &'a Cx, p: &Pkg) -> Result<Option<&'a LockedPkg>> {
    let source = p.source.to_string();
    if let Some(l) = cx.lock.pkg(&p.name, &source) {
        return Ok(Some(l));
    }
    if let Some(other) = cx.lock.named(&p.name).next() {
        bail!("fspec.lock has pkg:{} from `{}` but not from `{source}`; run `fspec update`", p.name, other.source);
    }
    Ok(None)
}

/// Trimmed stdout of a successful query, `None` when the command failed or printed nothing.
/// A rooted apply logs commands and answers with empty output, so empty never means an error.
fn query(cx: &Cx, cmd: &Cmd) -> Result<Option<String>> {
    let Output { status, stdout, .. } = cx.runner.run(cmd)?;
    let out = stdout.trim();
    Ok((status == 0 && !out.is_empty()).then(|| out.to_string()))
}

fn text(rec: &Map<String, Value>, key: &str) -> Option<String> {
    rec.get(key).and_then(Value::as_str).map(str::to_string)
}
