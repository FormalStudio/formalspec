//! `fspec update --cache`: record the digest of each CI-built artifact in the lock.

use std::path::Path;

use anyhow::{Context, Result, bail};
use fspec_core::lock::Lock;
use fspec_core::resource::{Host, Pkg, Resource, Source};
use fspec_core::runner::{Cmd, Runner};

use crate::ci::jobs;

/// `ghcr.io/<owner>/fspec-cache`, the owner taken from the `origin` remote of the repo holding
/// the spec (GitHub requires it lowercase), unless `registry` overrides the whole prefix.
pub fn registry(runner: &dyn Runner, spec_dir: &Path, registry: Option<&str>) -> Result<String> {
    if let Some(r) = registry {
        return Ok(r.trim_end_matches('/').to_string());
    }
    let out = runner.check(
        &Cmd::new("git").args(["-C"]).arg(spec_dir.display().to_string()).args(["remote", "get-url", "origin"]),
    )?;
    let url = out.stdout.trim();
    let path = url.split_once("github.com").map(|(_, rest)| rest.trim_start_matches([':', '/']));
    let owner = path.and_then(|p| p.split('/').next()).filter(|o| !o.is_empty());
    match owner {
        Some(o) => Ok(format!("ghcr.io/{}/fspec-cache", o.to_lowercase())),
        None => bail!("origin is `{url}`, not a GitHub remote, pass --registry"),
    }
}

/// For each locked git package and each platform a host declares, resolve the pushed tag of its
/// current build ([`crate::tag`]) to a digest and store `<registry>/<name>@<digest>` as the
/// artifact's `oci`, with the build key as its `build`. Returns one line per recorded digest; a
/// tag CI has not pushed yet goes to the error list and leaves the artifact as it was.
pub fn record(
    lock: &mut Lock,
    hosts: &[Host],
    registry: &str,
    runner: &dyn Runner,
) -> Result<(Vec<String>, Vec<String>)> {
    let (mut lines, mut errors) = (Vec::new(), Vec::new());
    for (name, platform) in jobs(hosts) {
        let Some(pkg) = git_pkg(hosts, &name) else { continue };
        let Source::Git { url, .. } = &pkg.source else { continue };
        let source = pkg.source.to_string();
        let Some(locked) = lock.pkg(&name, &source) else {
            errors.push(format!("{name}: not in fspec.lock, run fspec update"));
            continue;
        };
        let Some(rev) = locked.rev.clone() else {
            errors.push(format!("{name}: no rev in fspec.lock, run fspec update"));
            continue;
        };
        let identity = crate::identity(pkg, url, &rev, locked.patches.as_deref())?;
        let repo = format!("{registry}/{name}");
        let tagged = format!("{repo}:{}", crate::tag(&rev, &identity, &platform));
        let described = runner
            .check(&Cmd::new("oras").args(["manifest", "fetch", "--descriptor", &tagged]))
            .and_then(|o| digest(&o.stdout).with_context(|| format!("reading the descriptor of {tagged}")));
        match described {
            Ok(digest) => {
                let locked = lock.pkg(&name, &source).context("package vanished")?.clone();
                let mut locked = locked;
                let artifact = locked.artifacts.entry(platform.clone()).or_default();
                artifact.oci = Some(format!("{repo}@{digest}"));
                artifact.build = Some(crate::key(&identity));
                lock.upsert(locked);
                lines.push(format!("{name} {platform} {digest}"));
            }
            Err(e) => errors.push(format!("{name} {platform}: {e:#}")),
        }
    }
    Ok((lines, errors))
}

fn digest(descriptor: &str) -> Result<String> {
    let v: serde_json::Value = serde_json::from_str(descriptor)?;
    let digest = v.get("digest").and_then(|d| d.as_str()).context("no digest")?;
    if !digest.starts_with("sha256:") {
        bail!("digest `{digest}` is not sha256");
    }
    Ok(digest.into())
}

#[cfg(test)]
mod tests;

/// The first host's `git(...)` package `name`; its source is the lock's key.
fn git_pkg<'a>(hosts: &'a [Host], name: &str) -> Option<&'a Pkg> {
    hosts.iter().flat_map(|h| &h.resources).find_map(|r| match r {
        Resource::Pkg(p) if p.name == name && matches!(p.source, Source::Git { .. }) => Some(p),
        _ => None,
    })
}
