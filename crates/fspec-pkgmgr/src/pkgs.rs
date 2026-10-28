//! Install, cache, journal and drift logic shared by the four distro package backends and
//! the base packages of a classic distro.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use fspec_core::apply::{Cx, Op, Records};
use fspec_core::facts::Os;
use fspec_core::journal::{Journal, Undo, remove_any};
use fspec_core::par;
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

/// Names per batched version query, well under the kernel's argument limit.
const QUERY_CHUNK: usize = 1000;

/// The installed version of each of `names` that is installed. Batched queries run first, a few
/// thousand names in a handful of processes, and only the names they miss are queried one by
/// one. Several installed versions of one name (kernels) are joined with a space. A query that
/// succeeds without printing a version, as a rooted apply's runner does, maps the name to "".
pub(crate) fn live_versions(cx: &Cx, fam: Family, names: &[String]) -> Result<BTreeMap<String, String>> {
    let mut names: Vec<String> = names.iter().filter(|n| !n.starts_with('!')).cloned().collect();
    names.sort();
    names.dedup();
    let mut found: BTreeMap<String, String> = BTreeMap::new();
    let chunks: Vec<&[String]> = names.chunks(QUERY_CHUNK).collect();
    let batches = par::try_map(&chunks, |chunk| match fam.versions(chunk) {
        Some(cmd) => cx.runner.run(&cmd).map(|o| fam.parse_versions(&o.stdout)),
        None => Ok(vec![]),
    })?;
    for (n, v) in batches.into_iter().flatten().filter(|(n, _)| names.binary_search(n).is_ok()) {
        found.entry(n).and_modify(|have| *have = format!("{have} {v}")).or_insert(v);
    }
    let missed: Vec<&String> = names.iter().filter(|n| !found.contains_key(*n)).collect();
    let one_by_one = par::try_map(&missed, |n| -> Result<Option<String>> {
        let out = cx.runner.run(&fam.version(n))?;
        let v = fam.parse_version(n, &out.stdout);
        Ok((out.status == 0).then_some(v))
    })?;
    found.extend(missed.into_iter().zip(one_by_one).filter_map(|(n, v)| Some((n.clone(), v?))));
    Ok(found)
}

/// `{"names": [...], "versions": {name: version}}` from the live system.
pub(crate) fn record_names(cx: &Cx, fam: Family, names: &[String]) -> Result<Map<String, Value>> {
    Ok(names_record(names, &live_versions(cx, fam, names)?))
}

fn names_record(names: &[String], live: &BTreeMap<String, String>) -> Map<String, Value> {
    let versions: Map<String, Value> = names
        .iter()
        .filter_map(|n| live.get(n).filter(|v| !v.is_empty()).map(|v| (n.clone(), v.clone().into())))
        .collect();
    let mut rec = Map::new();
    rec.insert("names".into(), names.to_vec().into());
    if !versions.is_empty() {
        rec.insert("versions".into(), versions.into());
    }
    rec
}

/// Notes for names that are gone or at another version than the record says, and for `!name`
/// packages that are installed.
pub(crate) fn drift_names(cx: &Cx, fam: Family, names: &[String], rec: &Map<String, Value>) -> Result<Vec<String>> {
    Ok(names_drift(names, rec, &live_versions(cx, fam, &queried(names))?))
}

/// What [`drift_names`] asks about: the wanted names and the `!name` ones.
fn queried(names: &[String]) -> Vec<String> {
    names.iter().map(|n| n.strip_prefix('!').unwrap_or(n).to_string()).collect()
}

fn names_drift(names: &[String], rec: &Map<String, Value>, live: &BTreeMap<String, String>) -> Vec<String> {
    let mut notes = Vec::new();
    let versions = rec.get("versions").and_then(Value::as_object);
    for n in names {
        if let Some(gone) = n.strip_prefix('!') {
            if live.get(gone).is_some_and(|v| !v.is_empty()) {
                notes.push(format!("{gone} is installed"));
            }
            continue;
        }
        let Some(live) = live.get(n) else {
            notes.push(format!("{n} is not installed"));
            continue;
        };
        match versions.and_then(|v| v.get(n)).and_then(Value::as_str) {
            Some(r) if !live.is_empty() && live != r => notes.push(format!("{n} is {live}, applied {r}")),
            _ => {}
        }
    }
    notes
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

    /// Every entry's names go through one batched version query.
    pub fn drift_all(&self, cx: &Cx, entries: &[Entry]) -> Result<Vec<Vec<String>>> {
        let fam = self.0;
        let names: Vec<Vec<String>> = entries.iter().map(|e| entry_names(fam, e)).collect::<Result<_>>()?;
        let live = live_versions(cx, fam, &queried(&names.concat()))?;
        entries
            .iter()
            .zip(&names)
            .map(|(entry, names)| {
                let mut notes = names_drift(names, &entry.record, &live);
                if let Some(c) = want(fam, cx, pkg(&entry.resource)?)?.cached
                    && text(&entry.record, "file").as_deref() != Some(&c.file)
                {
                    notes.push(format!("lock moved to {}", c.file));
                }
                Ok(notes)
            })
            .collect()
    }

    /// On zypper, when the distro phase also changes the system, the new names go through one
    /// dry run together with the distro's, so a conflict between the two lists fails before the
    /// distro phase commits. Without a distro change nothing costly commits first, and the
    /// install reports the conflict itself.
    pub fn prepare(&self, cx: &Cx, ops: &[Op], staging: &Path) -> Result<()> {
        let mut fresh = Vec::new();
        for op in ops {
            let Some(d) = &op.desired else { continue };
            let p = pkg(&d.resource)?;
            let w = want(self.0, cx, p)?;
            if op.current.is_none() && from_repo(p).is_none() {
                fresh.extend(w.names.iter().cloned());
            }
            let Some(c) = w.cached else { continue };
            cache(cx, &d.resource.key().to_string(), &c)?;
        }
        if self.0 == Family::Zypper
            && !fresh.is_empty()
            && let Ok(distro) = fs::read_to_string(crate::zypper::staged_adds(staging))
        {
            let mut names: Vec<String> = distro.lines().filter(|l| !l.is_empty()).map(str::to_string).collect();
            names.extend(fresh);
            crate::zypper::check_solvable(cx, &names)?;
        }
        Ok(())
    }

    /// New packages installed by name go in one transaction, which on zypper or dnf saves a
    /// solver run and a repo load per package. When that transaction fails they are installed
    /// one at a time, so the ones that can land do and the error names the one that cannot.
    pub fn commit(&self, cx: &Cx, ops: &[Op], journal: &mut Journal) -> Result<Records> {
        let fam = self.0;
        let mut records = Records::new();
        let mut done: Vec<(&Op, Want)> = Vec::new();
        let mut fresh: Vec<(&Op, Want)> = Vec::new();
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
                None if w.cached.is_none() => {
                    fresh.push((op, w));
                    continue;
                }
                None => install(cx, fam, &w.names, w.cached.as_ref())?,
                Some((n, rec)) if n == w.names => upgrade(cx, journal, fam, &w, &rec)?,
                Some((n, rec)) => {
                    remove(cx, journal, fam, &n, &rec)?;
                    install(cx, fam, &w.names, w.cached.as_ref())?;
                }
            }
            done.push((op, w));
        }
        if !fresh.is_empty() {
            let all: Vec<String> = fresh.iter().flat_map(|(_, w)| w.names.clone()).collect();
            if fresh.len() == 1 || install(cx, fam, &all, None).is_err() {
                for (_, w) in &fresh {
                    install(cx, fam, &w.names, None)?;
                }
            }
            done.extend(fresh);
        }
        let all: Vec<String> = done.iter().flat_map(|(_, w)| w.names.clone()).collect();
        let live = live_versions(cx, fam, &all)?;
        for (op, w) in done {
            let mut rec = names_record(&w.names, &live);
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
                Ok(PkgEngine($fam).drift_all(cx, std::slice::from_ref(entry))?.remove(0))
            }

            fn drift_all(&self, cx: &Cx, entries: &[Entry]) -> Result<Vec<Vec<String>>> {
                PkgEngine($fam).drift_all(cx, entries)
            }

            fn prepare(&self, cx: &Cx, ops: &[Op], staging: &Path) -> Result<()> {
                PkgEngine($fam).prepare(cx, ops, staging)
            }

            fn commit(&self, cx: &Cx, ops: &[Op], _staging: &Path, journal: &mut Journal) -> Result<Records> {
                PkgEngine($fam).commit(cx, ops, journal)
            }
        }
    };
}
pub(crate) use family_backend;
