//! Install, cache, journal and drift logic shared by the four distro package backends and
//! the base packages of a classic distro.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use fspec_core::apply::{Cx, Op, Records};
use fspec_core::facts::Os;
use fspec_core::journal::{Journal, Undo, remove_any};
use fspec_core::resource::{Pkg, PkgMode, Resource, Source};
use fspec_core::runner::Cmd;
use fspec_core::state::Entry;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::family::Family;
use crate::{locked, pkg, text};

/// A locked package file: where it comes from and the file name it is cached under.
pub(crate) struct Cached {
    pub url: String,
    pub hash: String,
    pub file: String,
}

struct Want {
    names: Vec<String>,
    /// The cached file to install instead of the names. Only for a single-name package, since
    /// the lock holds one artifact per package.
    cached: Option<Cached>,
}

fn names(fam: Family, p: &Pkg) -> Result<Vec<String>> {
    let (Source::Dnf(d) | Source::Pacman(d) | Source::Apt(d) | Source::Xbps(d) | Source::Moss(d) | Source::Zypper(d)) =
        &p.source
    else {
        bail!("pkg:{} is not a {} package", p.name, fam.label())
    };
    Ok(if d.names.is_empty() { vec![p.name.clone()] } else { d.names.clone() })
}

fn want(fam: Family, cx: &Cx, p: &Pkg) -> Result<Want> {
    let names = names(fam, p)?;
    let artifact = locked(cx, p)?.and_then(|l| l.artifacts.get(&cx.facts.platform()));
    let cached = match (p.mode, artifact, names.len()) {
        (PkgMode::Pinned, Some(a), 1) => match (&a.url, &a.hash) {
            (Some(url), Some(hash)) => {
                let file = url.split(['?', '#']).next().and_then(|u| u.rsplit('/').next()).unwrap_or_default();
                if file.is_empty() || file.starts_with('.') {
                    bail!("pkg:{}: cannot derive a package file name from {url}", p.name);
                }
                Some(Cached { url: url.clone(), hash: hash.clone(), file: file.to_string() })
            }
            _ => None,
        },
        _ => None,
    };
    Ok(Want { names, cached })
}

pub(crate) fn cache_dir(cx: &Cx) -> PathBuf {
    cx.paths.system("/var/cache/fspec/pkgs")
}

fn sri(bytes: &[u8]) -> String {
    format!("sha256-{}", STANDARD.encode(Sha256::digest(bytes)))
}

/// `{"names": [...], "versions": {name: version}}` from the live system.
pub(crate) fn record_names(cx: &Cx, fam: Family, names: &[String]) -> Result<Map<String, Value>> {
    let mut versions = Map::new();
    for n in names.iter().filter(|n| !n.starts_with('!')) {
        let out = cx.runner.run(&fam.version(n))?;
        let v = fam.parse_version(n, &out.stdout);
        if out.status == 0 && !v.is_empty() {
            versions.insert(n.clone(), v.into());
        }
    }
    let mut rec = Map::new();
    rec.insert("names".into(), names.to_vec().into());
    if !versions.is_empty() {
        rec.insert("versions".into(), versions.into());
    }
    Ok(rec)
}

/// Notes for names that are gone or at another version than the record says.
pub(crate) fn drift_names(cx: &Cx, fam: Family, names: &[String], rec: &Map<String, Value>) -> Result<Vec<String>> {
    let mut notes = Vec::new();
    let versions = rec.get("versions").and_then(Value::as_object);
    for n in names {
        if let Some(gone) = n.strip_prefix('!') {
            if installed(cx, fam, gone)? {
                notes.push(format!("{gone} is installed"));
            }
            continue;
        }
        let out = cx.runner.run(&fam.version(n))?;
        if out.status != 0 {
            notes.push(format!("{n} is not installed"));
            continue;
        }
        let live = fam.parse_version(n, &out.stdout);
        match versions.and_then(|v| v.get(n)).and_then(Value::as_str) {
            Some(r) if !live.is_empty() && live != r => notes.push(format!("{n} is {live}, applied {r}")),
            _ => {}
        }
    }
    Ok(notes)
}

pub(crate) fn recorded_names(rec: &Map<String, Value>) -> Option<Vec<String>> {
    let list = rec.get("names")?.as_array()?;
    Some(list.iter().filter_map(Value::as_str).map(str::to_string).collect())
}

/// The package file the last apply installed, while it is still in the cache.
fn cached_file(cx: &Cx, rec: &Map<String, Value>) -> Option<PathBuf> {
    text(rec, "file").map(|f| cache_dir(cx).join(f)).filter(|p| p.exists())
}

/// zypper exits 107 when the packages installed but one of their rpm scriptlets failed. The
/// transaction is in, so the packages count as installed. A scriptlet usually fails because it
/// ran before a package from the same transaction it needs but does not require (1password-cli's
/// %post runs `groupadd` from shadow), so each such package is reinstalled once, with the whole
/// transaction in place.
const ZYPPER_SCRIPT_FAILED: i32 = 107;

fn run_all(cx: &Cx, cmds: &[Cmd]) -> Result<()> {
    for c in cmds {
        let ok: &[i32] = if c.program == "zypper" { &[0, ZYPPER_SCRIPT_FAILED] } else { &[0] };
        let out = cx.runner.check_ok(c, ok)?;
        if out.status != ZYPPER_SCRIPT_FAILED {
            continue;
        }
        let failed = failed_scriptlets(&format!("{}\n{}", out.stdout, out.stderr));
        if failed.is_empty() {
            eprintln!("fspec: `{c}` installed, but an rpm scriptlet failed; see /var/log/zypp/history");
            continue;
        }
        let retry = crate::family::zypper(["install", "--force", "--auto-agree-with-licenses"]).args(&failed);
        if cx.runner.check_ok(&retry, ok)?.status == ZYPPER_SCRIPT_FAILED {
            eprintln!("fspec: the scriptlets of {} failed again; see /var/log/zypp/history", failed.join(" "));
        }
    }
    Ok(())
}

/// The packages rpm names in `warning: %post(1password-cli-2.41.1-1.x86_64) scriptlet failed`.
fn failed_scriptlets(output: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in output.lines().filter(|l| l.contains("scriptlet failed")) {
        let Some(nevra) = line.split_once('(').and_then(|(_, rest)| rest.split_once(')')).map(|(n, _)| n) else {
            continue;
        };
        if !nevra.is_empty() && !out.iter().any(|n| n == nevra) {
            out.push(nevra.to_string());
        }
    }
    out
}

/// Records `cmds` so a rollback, which replays in reverse, runs them in the given order.
fn journal_all(journal: &mut Journal, cmds: Vec<Cmd>) -> Result<()> {
    cmds.into_iter().rev().try_for_each(|cmd| journal.record(Undo::Run { cmd }))
}

/// Journal what undoes the next change: the manager's own state from before it when it keeps
/// one, `cmds` otherwise.
fn journal_undo(cx: &Cx, journal: &mut Journal, fam: Family, cmds: impl FnOnce() -> Vec<Cmd>) -> Result<()> {
    match fam.restore_point(&cx.paths.system("/")) {
        Some(cmd) => journal.record(Undo::Run { cmd }),
        None => journal_all(journal, cmds()),
    }
}

fn install_cmds(cx: &Cx, fam: Family, names: &[String], cached: Option<&Cached>) -> Vec<Cmd> {
    match cached {
        Some(c) => fam.install_file(&cache_dir(cx).join(&c.file), names),
        None => fam.install_names(names),
    }
}

/// Install `names` (from `cached` when set). A `!name`, zypper's syntax, removes that package in
/// the same transaction, for one that conflicts with a wanted one. No undo is journaled: a failed
/// apply keeps what it installed, writes no state, and the next apply finds those packages already
/// there. Undoing would mean removing every new package, which for a first apply is thousands, and
/// reinstalling a `!name` the wanted set conflicts with fails anyway.
pub(crate) fn install(cx: &Cx, fam: Family, names: &[String], cached: Option<&Cached>) -> Result<()> {
    run_all(cx, &install_cmds(cx, fam, names, cached))
}

fn installed(cx: &Cx, fam: Family, name: &str) -> Result<bool> {
    let out = cx.runner.run(&fam.version(name))?;
    Ok(out.status == 0 && !fam.parse_version(name, &out.stdout).is_empty())
}

/// Installing over an existing package can only be reversed when the previous file is cached.
fn upgrade(cx: &Cx, journal: &mut Journal, fam: Family, w: &Want, rec: &Map<String, Value>) -> Result<()> {
    journal_undo(cx, journal, fam, || match cached_file(cx, rec) {
        Some(prev) => fam.install_file(&prev, &w.names),
        None => Vec::new(),
    })?;
    run_all(cx, &install_cmds(cx, fam, &w.names, w.cached.as_ref()))
}

/// Remove `names`, journaling the install that puts them back: the cached file when the record
/// names one that is still around, the names otherwise.
pub(crate) fn remove(
    cx: &Cx,
    journal: &mut Journal,
    fam: Family,
    names: &[String],
    rec: &Map<String, Value>,
) -> Result<()> {
    journal_undo(cx, journal, fam, || match cached_file(cx, rec) {
        Some(path) => fam.install_file(&path, names),
        None => fam.install_names(names),
    })?;
    cx.runner.check(&fam.remove(names))?;
    Ok(())
}

fn entry_names(fam: Family, e: &Entry) -> Result<Vec<String>> {
    match recorded_names(&e.record) {
        Some(n) => Ok(n),
        None => names(fam, pkg(&e.resource)?),
    }
}

/// A `pkg` backend for one distro family.
pub(crate) struct PkgEngine(pub Family);

impl PkgEngine {
    pub fn handles(&self, cx: &Cx, r: &Resource) -> bool {
        cx.facts.os == Os::Linux
            && !cx.facts.image
            && Family::of_distro(&cx.facts.distro) == Some(self.0)
            && matches!(r, Resource::Pkg(p) if Family::of_source(&p.source) == Some(self.0))
    }

    pub fn check(&self, cx: &Cx, r: &Resource) -> Result<()> {
        let p = pkg(r)?;
        if from_repo(p).is_some() && self.0 != Family::Zypper {
            bail!("pkg:{}: repo = ... names a zypper repo(...), and this is not a zypper distro", p.name);
        }
        want(self.0, cx, p).map(drop)
    }

    pub fn drift(&self, cx: &Cx, entry: &Entry) -> Result<Vec<String>> {
        let fam = self.0;
        let mut notes = drift_names(cx, fam, &entry_names(fam, entry)?, &entry.record)?;
        if let Some(c) = want(fam, cx, pkg(&entry.resource)?)?.cached
            && text(&entry.record, "file").as_deref() != Some(&c.file)
        {
            notes.push(format!("lock moved to {}", c.file));
        }
        Ok(notes)
    }

    pub fn prepare(&self, cx: &Cx, ops: &[Op]) -> Result<()> {
        for op in ops {
            let Some(d) = &op.desired else { continue };
            let Some(c) = want(self.0, cx, pkg(&d.resource)?)?.cached else { continue };
            cache(cx, &d.resource.key().to_string(), &c)?;
        }
        Ok(())
    }

    pub fn commit(&self, cx: &Cx, ops: &[Op], journal: &mut Journal) -> Result<Records> {
        let fam = self.0;
        let mut records = Records::new();
        for op in ops {
            let had = match &op.current {
                Some(e) => Some((entry_names(fam, e)?, e.record.clone())),
                None => None,
            };
            let Some(d) = &op.desired else {
                if let Some((n, rec)) = had {
                    remove(cx, journal, fam, &n, &rec)?;
                }
                continue;
            };
            let p = pkg(&d.resource)?;
            if let Some(alias) = from_repo(p)
                && !crate::zypper::repo_enabled(cx, alias)
            {
                records.insert(op.key.clone(), fspec_core::apply::deferred(format!("repo:{alias} is deferred")));
                continue;
            }
            let w = want(fam, cx, p)?;
            match had {
                None => install(cx, fam, &w.names, w.cached.as_ref())?,
                Some((n, rec)) if n == w.names => upgrade(cx, journal, fam, &w, &rec)?,
                Some((n, rec)) => {
                    remove(cx, journal, fam, &n, &rec)?;
                    install(cx, fam, &w.names, w.cached.as_ref())?;
                }
            }
            let mut rec = record_names(cx, fam, &w.names)?;
            if let Some(c) = &w.cached {
                rec.insert("file".into(), c.file.clone().into());
            }
            records.insert(op.key.clone(), rec);
        }
        Ok(records)
    }
}

/// The `repo(...)` a distro package says it comes from.
fn from_repo(p: &Pkg) -> Option<&str> {
    match &p.source {
        Source::Zypper(d) | Source::Dnf(d) | Source::Pacman(d) | Source::Apt(d) | Source::Xbps(d) | Source::Moss(d) => {
            d.repo.as_deref()
        }
        _ => None,
    }
}

/// Download `c` into the cache unless a file with the locked hash is already there.
fn cache(cx: &Cx, what: &str, c: &Cached) -> Result<()> {
    let dir = cache_dir(cx);
    let dest = dir.join(&c.file);
    if fs::read(&dest).is_ok_and(|b| sri(&b) == c.hash) {
        return Ok(());
    }
    let bytes = cx.fetch.get(&c.url)?;
    let got = sri(&bytes);
    if got != c.hash {
        bail!("{what}: downloaded {} hashes to {got}, the lock says {}", c.url, c.hash);
    }
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dest.with_extension("tmp");
    fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    if let Err(e) = fs::rename(&tmp, &dest) {
        remove_any(&tmp)?;
        return Err(e).with_context(|| format!("caching {}", dest.display()));
    }
    Ok(())
}

/// Declares a unit-struct backend that forwards to [`PkgEngine`].
macro_rules! family_backend {
    ($(#[$doc:meta])* $ty:ident, $name:literal, $fam:expr) => {
        $(#[$doc])*
        pub struct $ty;

        impl Backend for $ty {
            fn name(&self) -> &'static str {
                $name
            }

            /// On an image host these packages belong to `image(packages = ...)`.
            fn handles(&self, cx: &Cx, r: &Resource) -> bool {
                PkgEngine($fam).handles(cx, r)
            }

            fn phase(&self) -> Phase {
                Phase::Packages
            }

            fn check(&self, cx: &Cx, r: &Resource) -> Result<()> {
                PkgEngine($fam).check(cx, r)
            }

            fn drift(&self, cx: &Cx, entry: &Entry) -> Result<Vec<String>> {
                PkgEngine($fam).drift(cx, entry)
            }

            fn prepare(&self, cx: &Cx, ops: &[Op], _staging: &Path) -> Result<()> {
                PkgEngine($fam).prepare(cx, ops)
            }

            fn commit(&self, cx: &Cx, ops: &[Op], _staging: &Path, journal: &mut Journal) -> Result<Records> {
                PkgEngine($fam).commit(cx, ops, journal)
            }
        }
    };
}
pub(crate) use family_backend;
