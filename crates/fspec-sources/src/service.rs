//! Sources that name a version or commit and leave the artifact to their own tool.

use anyhow::{Result, anyhow, bail};
use fspec_core::lock::{Artifact, LockedPkg};
use fspec_core::runner::Cmd;
use serde_json::Value;

use crate::Cx;

/// The commit a remote's branch points at, read over HTTP from the OSTree repo's
/// `refs/heads`, so a host without flatpak can lock the flatpaks of every host. A remote fspec
/// has no URL for, and no `.flatpakrepo` to read one from, is asked through the local flatpak.
pub fn flatpak(
    id: &str,
    remote: &str,
    flatpakrepo: Option<&str>,
    platforms: &[String],
    cx: &Cx,
    out: &mut LockedPkg,
) -> Result<()> {
    let repo = match (remote_url(remote), flatpakrepo) {
        (Some(url), _) => url.to_string(),
        (None, Some(file)) => repo_url(file, cx)?,
        (None, None) => return flatpak_local(id, remote, cx, out),
    };
    let refs = match id.split('/').collect::<Vec<_>>()[..] {
        [name, arch, branch] => vec![format!("app/{name}/{arch}/{branch}"), format!("runtime/{name}/{arch}/{branch}")],
        [name] => vec![format!("app/{name}/{}/stable", flatpak_arch(platforms))],
        _ => bail!("flatpak `{id}` must be an app id or `<id>/<arch>/<branch>`"),
    };
    let mut last = None;
    for r in &refs {
        match cx.fetch.get(&format!("{repo}/refs/heads/{r}")) {
            Ok(body) => {
                let commit = String::from_utf8(body)?.trim().to_string();
                if commit.len() != 64 || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
                    bail!("{repo} has no commit for {r}: got {commit:?}");
                }
                out.artifacts.insert("any".into(), Artifact { commit: Some(commit), ..Default::default() });
                let name = id.split('/').next().unwrap_or(id);
                // The commit is the pin; the version is only shown, so a missing appstream entry is fine.
                out.version = (remote == "flathub")
                    .then(|| cx.fetch.get_json(&format!("https://flathub.org/api/v2/appstream/{name}")).ok())
                    .flatten()
                    .and_then(|doc| doc.pointer("/releases/0/version").and_then(Value::as_str).map(str::to_string));
                return Ok(());
            }
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| anyhow!("no ref to look up")).context(format!("{remote} has no {}", refs.join(" or "))))
}

/// The OSTree repo a `.flatpakrepo` file points at, from its `Url=` key.
fn repo_url(file: &str, cx: &Cx) -> Result<String> {
    let body = String::from_utf8(cx.fetch.get(file)?)?;
    body.lines()
        .find_map(|l| l.trim().strip_prefix("Url="))
        .map(|u| u.trim().trim_end_matches('/').to_string())
        .ok_or_else(|| anyhow!("{file} has no Url= line"))
}

fn remote_url(remote: &str) -> Option<&'static str> {
    match remote {
        "flathub" => Some("https://dl.flathub.org/repo"),
        "flathub-beta" => Some("https://dl.flathub.org/beta-repo"),
        _ => None,
    }
}

/// The arch of the Linux hosts that install it; flatpaks are only ever installed on Linux.
fn flatpak_arch(platforms: &[String]) -> &str {
    platforms.iter().find_map(|p| p.strip_suffix("-linux")).unwrap_or("x86_64")
}

fn flatpak_local(id: &str, remote: &str, cx: &Cx, out: &mut LockedPkg) -> Result<()> {
    let o = cx.runner.check(&Cmd::new("flatpak").args(["remote-info", "--system", remote, id]))?;
    let field = |key: &str| o.stdout.lines().find_map(|l| l.trim().strip_prefix(key)).map(|v| v.trim().to_string());
    let commit =
        field("Commit:").ok_or_else(|| anyhow!("`flatpak remote-info {remote} {id}` printed no Commit line"))?;
    out.version = field("Version:");
    out.artifacts.insert("any".into(), Artifact { commit: Some(commit), ..Default::default() });
    Ok(())
}

pub fn brew(name: &str, cask: bool, cx: &Cx, out: &mut LockedPkg) -> Result<()> {
    let url = format!("https://formulae.brew.sh/api/{}/{name}.json", if cask { "cask" } else { "formula" });
    let doc = cx.fetch.get_json(&url)?;
    let version = if cask { doc.get("version") } else { doc.pointer("/versions/stable") }.and_then(Value::as_str);
    out.version = Some(version.ok_or_else(|| anyhow!("{url} has no version"))?.to_string());
    Ok(())
}

pub fn nix(attr: &str, cx: &Cx, out: &mut LockedPkg) -> Result<()> {
    let o = cx.runner.check(&Cmd::new("nix").args(["eval", "--raw", &format!("nixpkgs#{attr}.version")]))?;
    let v = o.stdout.trim();
    if v.is_empty() {
        bail!("nixpkgs#{attr}.version evaluated to nothing");
    }
    out.version = Some(v.to_string());
    Ok(())
}
