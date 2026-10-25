use anyhow::{Result, bail};
use fspec_core::lock::LockedPkg;
use fspec_core::runner::Cmd;

use crate::Cx;

/// Pin `rev` (a branch, tag, `latest` for the newest version tag, or a full commit) to a commit.
/// A branch with `paths` pins its newest commit touching one of them.
pub fn resolve(url: &str, rev: &str, paths: &[String], cx: &Cx, out: &mut LockedPkg) -> Result<()> {
    if rev.len() == 40 && rev.bytes().all(|b| b.is_ascii_hexdigit()) {
        out.rev = Some(rev.to_lowercase());
        return Ok(());
    }
    if rev == "latest" {
        let o = cx.runner.check(&Cmd::new("git").args(["ls-remote", "--tags", "--sort=-v:refname", url]))?;
        let refs = parse(&o.stdout);
        let Some((_, first)) = refs.iter().find(|(_, r)| r.starts_with("refs/tags/")) else {
            bail!("{url} has no tags to take `latest` from");
        };
        let tag = first.trim_end_matches("^{}");
        let commit = refs
            .iter()
            .find(|(_, r)| r.strip_suffix("^{}") == Some(tag))
            .or_else(|| refs.iter().find(|(_, r)| *r == tag))
            .map(|(c, _)| c.clone());
        out.rev = commit;
        out.version = Some(tag.trim_start_matches("refs/tags/").to_string());
        return Ok(());
    }
    let o = cx.runner.check(&Cmd::new("git").args(["ls-remote", url, rev]))?;
    let refs = parse(&o.stdout);
    let wants =
        [format!("refs/heads/{rev}"), format!("refs/tags/{rev}^{{}}"), format!("refs/tags/{rev}"), rev.to_string()];
    let found = wants.iter().find_map(|w| refs.iter().find(|(_, r)| r == w)).map(|(c, _)| c.clone());
    let Some(tip) = found else { bail!("`git ls-remote {url} {rev}` found no matching ref") };
    let branch = refs.iter().any(|(_, r)| *r == format!("refs/heads/{rev}"));
    out.rev = Some(if branch && !paths.is_empty() { last_touching(url, rev, &tip, paths, cx)? } else { tip });
    Ok(())
}

/// The newest commit up to `tip` that changes one of `paths`, from a clone of the commits and
/// trees alone.
fn last_touching(url: &str, branch: &str, tip: &str, paths: &[String], cx: &Cx) -> Result<String> {
    let tmp = tempfile::tempdir()?;
    let dir = tmp.path().display().to_string();
    cx.runner.check(
        &Cmd::new("git")
            .args(["clone", "-q", "--bare", "--filter=blob:none", "--single-branch", "--branch", branch, url])
            .arg(&dir),
    )?;
    let o = cx.runner.check(&Cmd::new("git").args(["-C", &dir, "log", "-1", "--format=%H", tip, "--"]).args(paths))?;
    match o.stdout.trim() {
        "" => bail!("no commit on {branch} of {url} touches {}", paths.join(", ")),
        c => Ok(c.to_string()),
    }
}

fn parse(stdout: &str) -> Vec<(String, String)> {
    stdout
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(c, r)| (c.trim().to_string(), r.trim().to_string()))
        .collect()
}
