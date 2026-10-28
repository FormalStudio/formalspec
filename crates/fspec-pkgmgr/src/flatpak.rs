use std::path::Path;

use anyhow::{Result, bail};
use fspec_core::apply::{Backend, Cx, Op, Phase, Records};
use fspec_core::facts::Os;
use fspec_core::journal::{Journal, Undo};
use fspec_core::resource::{Pkg, PkgMode, Resource, Source};
use fspec_core::runner::Cmd;
use fspec_core::state::Entry;
use serde_json::Map;

use crate::{locked, pkg, query, text};

/// Flatpak apps installed system-wide and pinned to the locked commit.
pub struct FlatpakBackend;

struct App {
    id: String,
    remote: String,
}

struct Want {
    app: App,
    /// The locked commit to pin to; `None` for `self_managed`, which installs once.
    pin: Option<String>,
}

fn app(p: &Pkg) -> Result<App> {
    let Source::Flatpak { id, remote } = &p.source else { bail!("pkg:{} is not a flatpak", p.name) };
    Ok(App { id: id.clone().unwrap_or_else(|| p.name.clone()), remote: remote.clone() })
}

fn locked_commit(cx: &Cx, p: &Pkg) -> Result<Option<String>> {
    Ok(locked(cx, p)?.and_then(|l| l.artifacts.get("any")).and_then(|a| a.commit.clone()))
}

fn want(cx: &Cx, p: &Pkg) -> Result<Want> {
    let pin = match p.mode {
        PkgMode::Pinned => locked_commit(cx, p)?,
        PkgMode::SelfManaged => None,
    };
    Ok(Want { app: app(p)?, pin })
}

fn install_cmd(a: &App) -> Cmd {
    Cmd::new("flatpak").args(["install", "-y", "--noninteractive", "--system"]).arg(&a.remote).arg(&a.id)
}

fn pin_cmd(id: &str, commit: &str) -> Cmd {
    Cmd::new("flatpak").args(["update", "-y", "--noninteractive", "--system"]).arg(format!("--commit={commit}")).arg(id)
}

fn uninstall_cmd(id: &str) -> Cmd {
    Cmd::new("flatpak").args(["uninstall", "-y", "--noninteractive", "--system"]).arg(id)
}

fn live_commit(cx: &Cx, id: &str) -> Result<Option<String>> {
    query(cx, &Cmd::new("flatpak").args(["info", "--system", "--show-commit"]).arg(id))
}

/// No undo: a failed apply keeps what it installed (see `pkgs::install`).
fn install(cx: &Cx, w: &Want) -> Result<Option<String>> {
    cx.runner.check(&install_cmd(&w.app))?;
    match &w.pin {
        Some(c) => {
            cx.runner.check(&pin_cmd(&w.app.id, c))?;
            Ok(Some(c.clone()))
        }
        None => live_commit(cx, &w.app.id),
    }
}

/// Undo order is reverse of record order: install first, then pin back to the old commit.
fn uninstall(cx: &Cx, journal: &mut Journal, a: &App, recorded: Option<String>) -> Result<()> {
    if let Some(c) = live_commit(cx, &a.id)?.or(recorded) {
        journal.record(Undo::Run { cmd: pin_cmd(&a.id, &c) })?;
    }
    journal.record(Undo::Run { cmd: install_cmd(a) })?;
    cx.runner.check(&uninstall_cmd(&a.id))?;
    Ok(())
}

fn adjust(cx: &Cx, journal: &mut Journal, w: &Want, recorded: Option<String>) -> Result<Option<String>> {
    let live = live_commit(cx, &w.app.id)?;
    let Some(c) = &w.pin else { return Ok(live.or(recorded)) };
    if live.as_ref() == Some(c) {
        return Ok(Some(c.clone()));
    }
    if let Some(prev) = live.or(recorded) {
        journal.record(Undo::Run { cmd: pin_cmd(&w.app.id, &prev) })?;
    }
    cx.runner.check(&pin_cmd(&w.app.id, c))?;
    Ok(Some(c.clone()))
}

impl Backend for FlatpakBackend {
    fn name(&self) -> &'static str {
        "flatpak"
    }

    fn handles(&self, cx: &Cx, r: &Resource) -> bool {
        cx.facts.os == Os::Linux && matches!(r, Resource::Pkg(Pkg { source: Source::Flatpak { .. }, .. }))
    }

    fn phase(&self) -> Phase {
        Phase::Packages
    }

    fn check(&self, cx: &Cx, r: &Resource) -> Result<()> {
        let p = pkg(r)?;
        if p.mode == PkgMode::Pinned && locked_commit(cx, p)?.is_none() {
            bail!("pkg:{} has no locked flatpak commit; run `fspec update`", p.name);
        }
        Ok(())
    }

    fn drift(&self, cx: &Cx, entry: &Entry) -> Result<Vec<String>> {
        let p = pkg(&entry.resource)?;
        if p.mode != PkgMode::Pinned {
            return Ok(vec![]);
        }
        let a = app(p)?;
        let output = cx.runner.run(&Cmd::new("flatpak").args(["info", "--system", "--show-commit"]).arg(&a.id))?;
        if output.status != 0 {
            return Ok(vec![format!("{} is not installed", a.id)]);
        }
        let expected = locked_commit(cx, p)?.or_else(|| text(&entry.record, "commit"));
        let live = output.stdout.trim();
        Ok(match expected {
            Some(e) if !live.is_empty() && live != e => vec![format!("commit {live} is installed, {e} is locked")],
            _ => vec![],
        })
    }

    fn commit(&self, cx: &Cx, ops: &[Op], _staging: &Path, journal: &mut Journal) -> Result<Records> {
        let mut records = Records::new();
        for op in ops {
            let had = match &op.current {
                Some(e) => Some((app(pkg(&e.resource)?)?, text(&e.record, "commit"))),
                None => None,
            };
            let Some(d) = &op.desired else {
                if let Some((a, commit)) = had {
                    uninstall(cx, journal, &a, commit)?;
                }
                continue;
            };
            let w = want(cx, pkg(&d.resource)?)?;
            let commit = match had {
                None => install(cx, &w)?,
                Some((a, commit)) if a.id == w.app.id && a.remote == w.app.remote => adjust(cx, journal, &w, commit)?,
                Some((a, commit)) => {
                    uninstall(cx, journal, &a, commit)?;
                    install(cx, &w)?
                }
            };
            let mut rec = Map::new();
            rec.insert("id".into(), w.app.id.into());
            if let Some(c) = commit {
                rec.insert("commit".into(), c.into());
            }
            records.insert(op.key.clone(), rec);
        }
        Ok(records)
    }
}
