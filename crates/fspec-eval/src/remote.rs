//! `load("gh:owner/repo@ref//path.star", ...)`: a `.star` file from a GitHub repo, pinned in
//! `fspec.lock` like a package. Only `fspec update` resolves a ref; evaluation reads the locked
//! commit, from the spec's `.fspec/loads` (where a remote apply ships them), the user's cache, or
//! GitHub, and checks each file against its locked hash.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use fspec_core::fetch::Fetch;
use fspec_core::lock::{LockedLoad, load_rel};
use fspec_core::runner::{Cmd, Runner};
use sha2::{Digest, Sha256};

/// One parsed `gh:` load.
#[derive(Debug, PartialEq)]
pub struct Target<'a> {
    pub repo: &'a str,
    pub reference: Option<&'a str>,
    pub path: &'a str,
}

impl Target<'_> {
    /// The lock key: the repo and the ref as written.
    pub fn key(&self) -> String {
        match self.reference {
            Some(r) => format!("gh:{}@{r}", self.repo),
            None => format!("gh:{}", self.repo),
        }
    }
}

pub fn parse(load: &str) -> Result<Option<Target<'_>>> {
    let Some(rest) = load.strip_prefix("gh:") else { return Ok(None) };
    let (repo_ref, path) = rest
        .split_once("//")
        .with_context(|| format!("load(\"{load}\"): name the file after `//`, as in gh:owner/repo//fspec.star"))?;
    let (repo, reference) = match repo_ref.split_once('@') {
        Some((repo, r)) if !r.is_empty() => (repo, Some(r)),
        Some(_) => bail!("load(\"{load}\"): `@` needs a branch, tag or commit after it"),
        None => (repo_ref, None),
    };
    let mut parts = repo.split('/');
    let valid = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    if !matches!((parts.next(), parts.next(), parts.next()), (Some(o), Some(r), None) if valid(o) && valid(r)) {
        bail!("load(\"{load}\"): `{repo}` must be owner/repo");
    }
    check_path(load, path)?;
    Ok(Some(Target { repo, reference, path }))
}

fn check_path(load: &str, path: &str) -> Result<()> {
    if path.is_empty() || path.split('/').any(|c| c.is_empty() || c == "." || c == "..") || !path.ends_with(".star") {
        bail!("load(\"{load}\"): `{path}` must be a .star path inside the repo");
    }
    Ok(())
}

pub fn sri(body: &[u8]) -> String {
    format!("sha256-{}", STANDARD.encode(Sha256::digest(body)))
}

fn is_commit(r: &str) -> bool {
    r.len() == 40 && r.bytes().all(|b| b.is_ascii_hexdigit())
}

pub enum Mode<'a> {
    /// Evaluation: every load must be in the lock.
    Locked(&'a BTreeMap<String, LockedLoad>),
    /// `fspec update`: resolve each ref, or with `keep` reuse the locked commit when there is one.
    Resolve { runner: &'a dyn Runner, locked: &'a BTreeMap<String, LockedLoad>, keep: bool },
}

pub struct Remote<'a> {
    pub mode: Mode<'a>,
    pub fetch: &'a dyn Fetch,
    /// Searched in order for `<owner>/<repo>/<rev>/<path>`; a fetched file goes into the last.
    pub dirs: Vec<PathBuf>,
    /// What this evaluation loaded, keyed like the lock.
    pub loaded: RefCell<BTreeMap<String, LockedLoad>>,
}

impl Remote<'_> {
    /// The commit `key` is evaluated at.
    pub fn rev(&self, t: &Target) -> Result<String> {
        let key = t.key();
        if let Some(l) = self.loaded.borrow().get(&key) {
            return Ok(l.rev.clone());
        }
        let rev = match &self.mode {
            Mode::Locked(lock) => match lock.get(&key) {
                Some(l) => l.rev.clone(),
                None => bail!("fspec.lock has no entry for {key}; run `fspec update`"),
            },
            Mode::Resolve { locked, keep: true, .. } if locked.contains_key(&key) => locked[&key].rev.clone(),
            Mode::Resolve { runner, .. } => resolve(*runner, t)?,
        };
        self.loaded.borrow_mut().insert(key, LockedLoad { rev: rev.clone(), files: BTreeMap::new() });
        Ok(rev)
    }

    /// The text of `path` in the repo of `t` at `rev`, checked against the lock when evaluating.
    pub fn read(&self, t: &Target, rev: &str, path: &str) -> Result<String> {
        let key = t.key();
        let want = match &self.mode {
            Mode::Locked(lock) => Some(
                lock.get(&key)
                    .and_then(|l| l.files.get(path))
                    .with_context(|| format!("fspec.lock has no {path} for {key}; run `fspec update`"))?
                    .clone(),
            ),
            Mode::Resolve { .. } => None,
        };
        let rel = load_rel(t.repo, rev, path);
        let cached = self.dirs.iter().find_map(|d| std::fs::read(d.join(&rel)).ok());
        let body = match cached.filter(|b| want.as_deref().is_none_or(|w| sri(b) == w)) {
            Some(b) => b,
            None => {
                let url = format!("https://api.github.com/repos/{}/contents/{path}?ref={rev}", t.repo);
                let body = self
                    .fetch
                    .request(&url, &[("Accept", "application/vnd.github.raw")])
                    .with_context(|| format!("fetching {path} from {} at {rev}", t.repo))?
                    .body;
                if let Some(w) = &want
                    && sri(&body) != *w
                {
                    bail!("{path} from {key} at {rev} does not match fspec.lock; run `fspec update`");
                }
                if let Some(dir) = self.dirs.last() {
                    let to = dir.join(&rel);
                    // The cache only saves a fetch; a read-only or missing home is fine. Written
                    // whole, since another evaluation may be reading it.
                    let _ = fspec_core::flock::write_atomic(&to, &body);
                }
                body
            }
        };
        if let Some(l) = self.loaded.borrow_mut().get_mut(&key) {
            l.files.insert(path.to_string(), sri(&body));
        }
        String::from_utf8(body).map_err(|_| anyhow!("{path} from {key} is not UTF-8"))
    }
}

/// The commit a ref names: a full commit as is, else what `git ls-remote` finds (`HEAD` for the
/// default branch), as a `git(...)` source resolves one.
fn resolve(runner: &dyn Runner, t: &Target) -> Result<String> {
    let reference = t.reference.unwrap_or("HEAD");
    if is_commit(reference) {
        return Ok(reference.to_ascii_lowercase());
    }
    let url = format!("https://github.com/{}", t.repo);
    let out = runner.check(&Cmd::new("git").args(["ls-remote", &url, reference]))?;
    let mut lines = out.stdout.lines().filter_map(|l| l.split_once('\t'));
    // A tag lists its peeled commit as `<tag>^{}` after the tag object.
    let pick = lines.clone().find(|(_, r)| r.ends_with("^{}")).or_else(|| lines.next());
    match pick {
        Some((rev, _)) if is_commit(rev) => Ok(rev.to_string()),
        _ => bail!("`git ls-remote {url} {reference}` found no matching ref"),
    }
}
