use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use fspec_core::apply::{Backend, Cx, Op, Phase, Records};
use fspec_core::facts::Os;
use fspec_core::journal::Journal;
use fspec_core::par;
use fspec_core::resource::{Pkg, Resource, Source};
use fspec_core::runner::Cmd;
use fspec_core::state::Entry;
use serde::Deserialize;
use serde_json::Map;

use crate::{locked, pkg, text};

/// Mac apps on Linux: the locked DMG or archive, handed to atoll as the user. atoll copies the
/// bundle into `$XDG_DATA_HOME/atoll/Applications` and writes its desktop file and icon.
pub struct AtollBackend;

/// One app as `atoll install --json` prints it and `atoll list --json` lists it.
#[derive(Deserialize, Debug)]
struct App {
    bundle_id: String,
    #[serde(default)]
    app: Option<String>,
    #[serde(default)]
    desktop: Option<String>,
    /// The app's own state, which the engine moves to the trash when the package goes.
    #[serde(default)]
    data: Vec<String>,
}

/// The locked url and hash for this host's CPU, or the one build for every host.
fn artifact<'a>(cx: &'a Cx, p: &Pkg) -> Result<(&'a str, &'a str)> {
    let l = locked(cx, p)?.with_context(|| format!("pkg {} is not in fspec.lock, run fspec update", p.name))?;
    l.artifacts
        .get(&cx.facts.platform())
        .or_else(|| l.artifacts.get("any"))
        .and_then(|a| Some((a.url.as_deref()?, a.hash.as_deref()?)))
        .with_context(|| format!("pkg {} has no url and hash in fspec.lock, run fspec update", p.name))
}

fn staged(staging: &Path, p: &Pkg) -> PathBuf {
    staging.join("atoll").join(&p.name)
}

/// What `atoll install` takes from the staged download: the `.dmg` itself, or the one `.app`
/// an archive held at its top or inside its single top-level directory.
fn bundle(dir: &Path) -> Result<PathBuf> {
    let image = dir.join("app.dmg");
    if image.is_file() {
        return Ok(image);
    }
    let unpacked = dir.join("unpacked");
    let apps = |d: &Path| -> Vec<PathBuf> {
        let mut found: Vec<PathBuf> = fs::read_dir(d)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir() && p.extension().is_some_and(|x| x == "app"))
            .collect();
        found.sort();
        found
    };
    let mut found = apps(&unpacked);
    if found.is_empty() {
        let tops: Vec<PathBuf> = fs::read_dir(&unpacked).into_iter().flatten().flatten().map(|e| e.path()).collect();
        if let [only] = tops.as_slice() {
            found = apps(only);
        }
    }
    match found.as_slice() {
        [one] => Ok(one.clone()),
        [] => bail!("the download holds no .app bundle"),
        many => {
            let names: Vec<_> = many.iter().filter_map(|p| p.file_name()).map(|n| n.to_string_lossy()).collect();
            bail!("the download holds {} .app bundles: {}", many.len(), names.join(", "))
        }
    }
}

fn install_cmd(path: &Path) -> Cmd {
    Cmd::new("atoll").args(["install", "--json"]).arg(path.display().to_string())
}

fn uninstall_cmd(bundle_id: &str) -> Cmd {
    Cmd::new("atoll").arg("uninstall").arg(bundle_id)
}

/// Every app atoll has installed, `None` when atoll answered nothing (a rooted apply only logs).
fn installed(cx: &Cx) -> Result<Option<Vec<App>>> {
    let out = cx.runner.check(&Cmd::new("atoll").args(["list", "--json"]))?;
    if out.stdout.trim().is_empty() {
        return Ok(None);
    }
    serde_json::from_str(&out.stdout).map(Some).context("reading `atoll list --json`")
}

/// atoll's word for what it installed, which the record keeps for the removal; `None` when
/// atoll answered nothing (a rooted apply only logs).
fn install(cx: &Cx, path: &Path) -> Result<Option<App>> {
    let out = cx.runner.check(&install_cmd(path))?;
    if out.stdout.trim().is_empty() {
        return Ok(None);
    }
    serde_json::from_str(out.stdout.trim()).map(Some).with_context(|| {
        format!("`atoll install --json` printed no app with a bundle_id (it printed `{}`)", out.stdout.trim())
    })
}

impl Backend for AtollBackend {
    fn name(&self) -> &'static str {
        "atoll"
    }

    fn handles(&self, _cx: &Cx, r: &Resource) -> bool {
        matches!(r, Resource::Pkg(Pkg { source: Source::Mac { .. }, .. }))
    }

    fn phase(&self) -> Phase {
        Phase::Packages
    }

    fn check(&self, cx: &Cx, r: &Resource) -> Result<()> {
        let p = pkg(r)?;
        if cx.facts.os != Os::Linux {
            bail!(
                "mac(...) installs through atoll, which runs Mac apps on Linux; on macOS take the DMG with \
                 github(..., asset = \"*.dmg\") or url(...) and apps = [...]"
            );
        }
        let unused = [
            ("patches", p.patches.is_empty()),
            ("build_flags", p.build_flags.is_empty()),
            ("build", p.build.is_empty()),
            ("bins", p.bins.is_empty()),
            ("links", p.links.is_empty()),
            ("share", p.share.is_empty()),
            ("etc", p.etc.is_empty()),
            ("parts", p.parts.is_empty()),
            ("apps", p.apps.is_empty()),
        ];
        if let Some((field, _)) = unused.iter().find(|(_, empty)| !empty) {
            bail!("mac(...) hands the app to atoll whole, so {field} does not apply");
        }
        artifact(cx, p)?;
        if cx.runner.run(&Cmd::new("sh").args(["-c", "command -v atoll"]))?.status != 0 {
            bail!("atoll is not on PATH; mac(...) apps install through it, so install atoll first");
        }
        Ok(())
    }

    /// Gone from atoll, or installed from an artifact the lock has moved past. `self_managed`
    /// changes nothing: Sparkle under atoll finds no update, so the lock is the only updater.
    fn drift_all(&self, cx: &Cx, entries: &[Entry]) -> Result<Vec<Vec<String>>> {
        let live = match installed(cx) {
            Ok(live) => live,
            Err(e) => return Ok(entries.iter().map(|_| vec![format!("atoll cannot list its apps: {e:#}")]).collect()),
        };
        entries
            .iter()
            .map(|e| {
                let p = pkg(&e.resource)?;
                let Some(id) = text(&e.record, "bundle_id") else { return Ok(vec!["no install recorded".into()]) };
                if live.as_ref().is_some_and(|apps| !apps.iter().any(|a| a.bundle_id == id)) {
                    return Ok(vec![format!("{id} is not installed")]);
                }
                let Some(lock) = locked(cx, p)? else { return Ok(vec![]) };
                let Ok((_, hash)) = artifact(cx, p) else { return Ok(vec![]) };
                if text(&e.record, "hash").as_deref() == Some(hash) {
                    return Ok(vec![]);
                }
                let had = text(&e.record, "version").unwrap_or_else(|| "another build".into());
                let new = lock.version.clone().unwrap_or_else(|| "a new build".into());
                Ok(vec![format!("version {had} is installed, {new} is locked")])
            })
            .collect()
    }

    /// Download and verify every app before anything changes, and find what atoll takes.
    fn prepare(&self, cx: &Cx, ops: &[Op], staging: &Path) -> Result<()> {
        let wanted: Vec<&Pkg> =
            ops.iter().filter_map(|op| op.desired.as_ref()).map(|d| pkg(&d.resource)).collect::<Result<_>>()?;
        par::try_map(&wanted, |p| -> Result<()> {
            let (url, hash) = artifact(cx, p)?;
            let body = cx.fetch.get(url).with_context(|| format!("pkg {}: downloading {url}", p.name))?;
            fspec_store::archive::verify(&body, hash).with_context(|| format!("pkg {}: {url}", p.name))?;
            let dir = staged(staging, p);
            fs::create_dir_all(&dir)?;
            if fspec_store::archive::is_dmg(&body) {
                fs::write(dir.join("app.dmg"), &body)?;
            } else if fspec_store::archive::is_archive(&body) {
                fspec_store::archive::unpack(&body, &dir.join("unpacked"), &p.name)?;
            } else {
                bail!("pkg {}: {url} is neither a .dmg nor an archive holding a .app", p.name);
            }
            bundle(&dir).map(drop).with_context(|| format!("pkg {}: {url}", p.name))
        })?;
        Ok(())
    }

    /// No undo is journaled: atoll keeps no copy of an app it replaced or removed to put back, so
    /// a failed apply keeps what atoll did and the next one converges from there.
    fn commit(&self, cx: &Cx, ops: &[Op], staging: &Path, _journal: &mut Journal) -> Result<Records> {
        let mut records = Records::new();
        let had = |op: &Op| op.current.as_ref().and_then(|e| text(&e.record, "bundle_id"));
        // An atoll that cannot list its apps cannot uninstall them either (it may be removed
        // in this same apply), so its apps count as gone rather than blocking every apply.
        if ops.iter().any(|op| op.is_removal())
            && let Ok(live) = installed(cx)
        {
            for (op, id) in ops.iter().filter(|op| op.is_removal()).filter_map(|op| had(op).map(|id| (op, id))) {
                // Removed by hand since the last apply: nothing left to uninstall.
                if live.as_ref().is_none_or(|apps| apps.iter().any(|a| a.bundle_id == id)) {
                    cx.runner.check(&uninstall_cmd(&id)).with_context(|| format!("removing {}", op.key))?;
                }
            }
        }
        for op in ops {
            let Some(d) = &op.desired else { continue };
            let p = pkg(&d.resource)?;
            let (_, hash) = artifact(cx, p)?;
            let Some(app) =
                install(cx, &bundle(&staged(staging, p))?).with_context(|| format!("installing {}", op.key))?
            else {
                continue;
            };
            if let Some(old) = had(op).filter(|old| *old != app.bundle_id) {
                // A failed apply may have removed it already.
                if installed(cx)?.is_none_or(|apps| apps.iter().any(|a| a.bundle_id == old)) {
                    cx.runner.check(&uninstall_cmd(&old))?;
                }
            }
            let mut rec = Map::new();
            rec.insert("bundle_id".into(), app.bundle_id.clone().into());
            for (key, value) in [("app", app.app), ("desktop", app.desktop)] {
                if let Some(v) = value {
                    rec.insert(key.into(), v.into());
                }
            }
            if !app.data.is_empty() {
                // atoll answers real paths; the record keeps spec paths (`~/...`) so a rooted
                // apply resolves them under its root.
                // atoll keys every data path by bundle id; anything else (`~`, a folder all its
                // apps share) is not this app's to trash.
                let data: Vec<String> = app
                    .data
                    .iter()
                    .filter(|d| Path::new(d).file_name().is_some_and(|n| n.to_string_lossy().contains(&app.bundle_id)))
                    .map(|d| cx.paths.to_spec(Path::new(d)))
                    .filter(|d| d.starts_with("~/"))
                    .collect();
                rec.insert("data".into(), data.into());
            }
            if let Some(v) = locked(cx, p)?.and_then(|l| l.version.clone()) {
                rec.insert("version".into(), v.into());
            }
            rec.insert("hash".into(), hash.into());
            records.insert(op.key.clone(), rec);
        }
        Ok(records)
    }
}
