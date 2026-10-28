use std::path::Path;

use anyhow::{Result, bail};
use fspec_core::apply::{Backend, Cx, Op, Phase, Records};
use fspec_core::facts::Os;
use fspec_core::journal::{Journal, Undo};
use fspec_core::resource::{Pkg, PkgMode, Resource, Source};
use fspec_core::runner::Cmd;
use fspec_core::state::Entry;
use serde_json::Map;

use crate::{locked, pkg, query};

/// Homebrew formulae and casks, user scope (Homebrew refuses to run as root).
pub struct BrewBackend;

#[derive(PartialEq)]
struct Formula {
    name: String,
    cask: bool,
    tap: Option<String>,
}

struct Want {
    formula: Formula,
    pinned: bool,
    /// Version from the lock; Homebrew installs the current one, so this only drives drift.
    locked: Option<String>,
}

fn formula(p: &Pkg) -> Result<Formula> {
    let Source::Brew { name, cask, tap } = &p.source else { bail!("pkg:{} is not a brew package", p.name) };
    if let Some(t) = tap
        && (t.split('/').count() != 2 || t.split('/').any(str::is_empty))
    {
        bail!("pkg:{}: tap `{t}` must be `owner/repo`", p.name);
    }
    Ok(Formula { name: name.clone().unwrap_or_else(|| p.name.clone()), cask: *cask, tap: tap.clone() })
}

fn want(cx: &Cx, p: &Pkg) -> Result<Want> {
    let locked = locked(cx, p)?.and_then(|l| l.version.clone());
    Ok(Want { formula: formula(p)?, pinned: p.mode == PkgMode::Pinned, locked })
}

fn cmd(verb: &str, f: &Formula) -> Cmd {
    let c = Cmd::new("brew").arg(verb);
    if f.cask { c.arg("--cask") } else { c }.arg(&f.name)
}

fn list_cmd(f: &Formula) -> Cmd {
    let c = Cmd::new("brew").arg("list");
    if f.cask { c.arg("--cask") } else { c }.args(["--versions", &f.name])
}

/// `brew list --versions` prints `name 1.2.3` and several versions space separated; the newest is last.
fn version(cx: &Cx, f: &Formula) -> Result<Option<String>> {
    Ok(query(cx, &list_cmd(f))?.and_then(|l| l.split_whitespace().last().map(str::to_string)))
}

/// Homebrew appends `_N` to the version when a formula is rebuilt; the lock holds the upstream version.
fn same_version(installed: &str, locked: &str) -> bool {
    let base = match installed.rsplit_once('_') {
        Some((base, n)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => base,
        _ => installed,
    };
    installed == locked || base == locked
}

/// No undo: a failed apply keeps what it installed (see `pkgs::install`).
fn install(cx: &Cx, w: &Want) -> Result<Option<String>> {
    let f = &w.formula;
    match &f.tap {
        // The tap stays when the package goes: other packages may come from it.
        Some(tap) => {
            cx.runner.check(&Cmd::new("brew").args(["tap", tap]))?;
            let c = Cmd::new("brew").arg("install");
            cx.runner.check(&if f.cask { c.arg("--cask") } else { c }.arg(format!("{tap}/{}", f.name)))?;
        }
        None => {
            cx.runner.check(&cmd("install", f))?;
        }
    }
    version(cx, f)
}

fn uninstall(cx: &Cx, journal: &mut Journal, f: &Formula) -> Result<()> {
    journal.record(Undo::Run { cmd: cmd("install", f) })?;
    cx.runner.check(&cmd("uninstall", f))?;
    Ok(())
}

/// No undo is journaled for an upgrade: Homebrew cannot reinstall the previous version.
fn adjust(cx: &Cx, w: &Want) -> Result<Option<String>> {
    let live = version(cx, &w.formula)?;
    let behind = match (&live, &w.locked) {
        (Some(l), Some(want)) => !same_version(l, want),
        _ => false,
    };
    if w.pinned && behind {
        cx.runner.check(&cmd("upgrade", &w.formula))?;
        return version(cx, &w.formula);
    }
    Ok(live)
}

impl Backend for BrewBackend {
    fn name(&self) -> &'static str {
        "brew"
    }

    fn handles(&self, cx: &Cx, r: &Resource) -> bool {
        cx.facts.os == Os::Darwin && matches!(r, Resource::Pkg(Pkg { source: Source::Brew { .. }, .. }))
    }

    fn phase(&self) -> Phase {
        Phase::Packages
    }

    fn check(&self, cx: &Cx, r: &Resource) -> Result<()> {
        locked(cx, pkg(r)?).map(drop)
    }

    fn drift(&self, cx: &Cx, entry: &Entry) -> Result<Vec<String>> {
        let w = want(cx, pkg(&entry.resource)?)?;
        let output = cx.runner.run(&list_cmd(&w.formula))?;
        if output.status != 0 {
            return Ok(vec![format!("{} is not installed", w.formula.name)]);
        }
        let live = output.stdout.split_whitespace().last();
        Ok(match (w.pinned, live, &w.locked) {
            (true, Some(l), Some(want)) if !same_version(l, want) => {
                vec![format!("version {l} is installed, {want} is locked")]
            }
            _ => vec![],
        })
    }

    fn commit(&self, cx: &Cx, ops: &[Op], _staging: &Path, journal: &mut Journal) -> Result<Records> {
        let mut records = Records::new();
        for op in ops {
            let had = match &op.current {
                Some(e) => Some(formula(pkg(&e.resource)?)?),
                None => None,
            };
            let Some(d) = &op.desired else {
                if let Some(f) = had {
                    uninstall(cx, journal, &f)?;
                }
                continue;
            };
            let w = want(cx, pkg(&d.resource)?)?;
            let installed = match had {
                None => install(cx, &w)?,
                Some(f) if f == w.formula => adjust(cx, &w)?,
                Some(f) => {
                    uninstall(cx, journal, &f)?;
                    install(cx, &w)?
                }
            };
            let mut rec = Map::new();
            rec.insert("name".into(), w.formula.name.into());
            if let Some(v) = installed {
                rec.insert("version".into(), v.into());
            }
            records.insert(op.key.clone(), rec);
        }
        Ok(records)
    }
}
