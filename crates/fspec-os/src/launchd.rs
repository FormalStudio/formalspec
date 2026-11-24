//! launchd plists for `service` and `timer` resources.
//!
//! `exec` becomes `ProgramArguments` through [`argv`](crate::argv).
//! Launchd has no ordering between jobs and no description key, so `after` and `description`
//! have no effect there.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use fspec_core::apply::{Backend, Cx, Op, Phase, Records};
use fspec_core::facts::Init;
use fspec_core::journal::{Journal, Undo, remove_any};
use fspec_core::resource::{Restart, Service, Timer};
use fspec_core::runner::Cmd;
use fspec_core::state::Entry;
use fspec_core::{Resource, Scope};
use plist::{Dictionary, Value};
use serde_json::{Map, json};

use crate::cron::Schedule;
use crate::{argv, check_name, read_or_empty, reject_init_extras, write_file};

pub struct LaunchdBackend;

impl Backend for LaunchdBackend {
    fn name(&self) -> &'static str {
        "launchd"
    }

    fn handles(&self, cx: &Cx, r: &Resource) -> bool {
        cx.facts.init == Init::Launchd && matches!(r, Resource::Service(_) | Resource::Timer(_))
    }

    fn phase(&self) -> Phase {
        Phase::Services
    }

    fn check(&self, cx: &Cx, r: &Resource) -> Result<()> {
        if let Resource::Service(s) = r
            && let Some(s) = crate::distro_unit::of(s)
        {
            return crate::distro_unit::check(Init::Launchd, s);
        }
        reject_init_extras(cx.facts.init, r)?;
        render(r).map(drop)
    }

    fn drift(&self, cx: &Cx, entry: &Entry) -> Result<Vec<String>> {
        let path = plist_path(&entry.resource);
        let mut notes = match read_or_empty(&cx.resolve(&path)?) {
            None => vec![format!("{path} is missing")],
            Some(on_disk) if on_disk != render(&entry.resource)? => vec![format!("{path} was edited")],
            Some(_) => vec![],
        };
        notes.extend(stale(cx, &entry.resource)?);
        Ok(notes)
    }

    fn prepare(&self, _cx: &Cx, ops: &[Op], staging: &Path) -> Result<()> {
        for op in ops.iter().filter(|o| !o.is_removal()) {
            write_file(&staged(staging, op), &render(op.resource())?)?;
        }
        Ok(())
    }

    fn commit(&self, cx: &Cx, ops: &[Op], staging: &Path, journal: &mut Journal) -> Result<Records> {
        let mut records = Records::new();
        for op in ops {
            let r = op.resource();
            let spec_path = plist_path(r);
            let path = cx.resolve(&spec_path)?;
            let path_arg = path.to_string_lossy().into_owned();
            let domain = domain(cx, r)?;
            let target = format!("{domain}/{}", label(r));
            let bootstrap = Cmd::new("launchctl").args(["bootstrap", &domain, &path_arg]);
            let bootout = Cmd::new("launchctl").args(["bootout", &target]);

            if op.is_removal() {
                // Replayed backwards: the plist comes back, then the job is loaded again.
                journal.record(Undo::Run { cmd: bootstrap })?;
                journal.backup(&path)?;
                cx.runner.run(&bootout)?;
                remove_any(&path)?;
                continue;
            }

            let rendered = fs::read_to_string(staged(staging, op))?;
            // A job changed only because its program moved keeps its plist. bootout returns before
            // the process exits, so a bootstrap right after it fails with EIO; kickstart -k waits.
            if op.current.is_some() && read_or_empty(&path).as_deref() == Some(rendered.as_str()) {
                let kickstart = Cmd::new("launchctl").args(["kickstart", "-k", &target]);
                journal.record(Undo::Run { cmd: kickstart.clone() })?;
                cx.runner.check(&kickstart)?;
                let mut rec = Map::new();
                rec.insert("plist".into(), json!(spec_path));
                records.insert(op.key.clone(), rec);
                continue;
            }
            if op.current.is_some() {
                journal.record(Undo::Run { cmd: bootstrap.clone() })?;
            }
            journal.backup(&path)?;
            write_file(&path, &rendered)?;
            if op.current.is_some() {
                // A job that is not loaded (never started, or booted out by hand) fails here.
                cx.runner.run(&bootout)?;
            }
            journal.record(Undo::Run { cmd: bootout })?;
            load(cx, &bootstrap)?;
            let mut rec = Map::new();
            rec.insert("plist".into(), json!(spec_path));
            records.insert(op.key.clone(), rec);
        }
        Ok(records)
    }

    fn refresh(&self, cx: &Cx, entries: &[Entry], journal: &mut Journal) -> Result<Records> {
        let mut records = Records::new();
        for e in entries {
            if let Some(why) = stale(cx, &e.resource)?
                && let Some(failed) = restart(cx, journal, &e.resource, &why)?
            {
                let mut rec = e.record.clone();
                fspec_core::apply::pending(&mut rec, failed);
                records.insert(e.resource.key(), rec);
            }
        }
        Ok(records)
    }
}

/// Why a running service is stale: the job's program now resolves to another store package than
/// the one its process was started from, as after an apply that moved the package's `current`
/// link. The commit restarts a drifted service with `kickstart -k`, which starts the new program.
fn stale(cx: &Cx, r: &Resource) -> Result<Option<String>> {
    let Resource::Service(s) = r else { return Ok(None) };
    let store = cx.paths.fspec_root().join("store");
    if crate::distro_unit::of(s).is_some() || !store.is_dir() {
        return Ok(None);
    }
    let target = format!("{}/{}", domain(cx, r)?, label(r));
    // A job that is not loaded fails here, and one that is not running prints no pid. Nested
    // sections indent deeper, so only the job's own keys sit behind one tab.
    let shown = cx.runner.run(&Cmd::new("launchctl").args(["print", &target]))?.stdout;
    let prop = |k: &str| shown.lines().find_map(|l| l.strip_prefix('\t')?.strip_prefix(k)?.strip_prefix(" = "));
    let (Some(pid), Some(program)) = (prop("pid"), prop("program")) else { return Ok(None) };
    // `ps` prints the path the process was started by, the `current` link that already points at
    // the new package. lsof lists the executable first among the text files, resolved.
    let txt = cx.runner.run(&Cmd::new("lsof").args(["-a", "-p", pid, "-d", "txt", "-Fn"]))?.stdout;
    let Some(running) = txt.lines().find_map(|l| l.strip_prefix('n')) else { return Ok(None) };
    crate::store_moved(cx, &store, &label(r), running, program)
}

/// Restart a stale job. Returns why the restart failed, which leaves the service pending instead
/// of rolling back the apply.
fn restart(cx: &Cx, journal: &mut Journal, r: &Resource, why: &str) -> Result<Option<String>> {
    let kickstart = Cmd::new("launchctl").args(["kickstart", "-k", &format!("{}/{}", domain(cx, r)?, label(r))]);
    journal.record(Undo::Run { cmd: kickstart.clone() })?;
    let out = cx.runner.run(&kickstart)?;
    if out.status != 0 {
        let msg = out.stderr.lines().chain(out.stdout.lines()).find(|l| !l.trim().is_empty()).unwrap_or("");
        return Ok(Some(format!("`{kickstart}` failed: {}", msg.trim())));
    }
    println!("{}: restarted, {why}", r.key());
    Ok(None)
}

/// `bootout` returns while a job that is slow to exit is still being torn down, and a `bootstrap`
/// of the same label in that window fails with 5 (EIO). Retry for up to 10 s. A rooted run only
/// logs commands, so it does not wait.
fn load(cx: &Cx, bootstrap: &Cmd) -> Result<()> {
    if cx.paths.root == Path::new("/") {
        for _ in 0..40 {
            if cx.runner.run(bootstrap)?.status != 5 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    }
    cx.runner.check(bootstrap).map(drop)
}

fn staged(staging: &Path, op: &Op) -> std::path::PathBuf {
    staging.join("launchd").join(format!("{}.plist", op.key.to_string().replace(':', "-")))
}

fn name(r: &Resource) -> &str {
    match r {
        Resource::Service(s) => &s.name,
        Resource::Timer(t) => &t.name,
        _ => unreachable!("launchd claims services and timers"),
    }
}

fn label(r: &Resource) -> String {
    format!("fspec.{}", name(r))
}

fn plist_path(r: &Resource) -> String {
    let dir = if r.scope() == Scope::User { "~/Library/LaunchAgents" } else { "/Library/LaunchDaemons" };
    format!("{dir}/{}.plist", label(r))
}

/// `system` for daemons, `gui/<uid>` for the applying user's agents.
fn domain(cx: &Cx, r: &Resource) -> Result<String> {
    if r.scope() == Scope::System {
        return Ok("system".into());
    }
    let out = cx.runner.check(&Cmd::new("id").arg("-u"))?;
    let uid = out.stdout.trim();
    // A rooted run only logs commands, so nothing answers.
    if uid.is_empty() && cx.paths.root != Path::new("/") {
        return Ok("gui/0".into());
    }
    let uid: u32 = uid.parse().with_context(|| format!("`id -u` printed `{uid}`"))?;
    Ok(format!("gui/{uid}"))
}

fn render(r: &Resource) -> Result<String> {
    let (name, kind) = match r {
        Resource::Service(s) => (&s.name, "service"),
        Resource::Timer(t) => (&t.name, "timer"),
        _ => unreachable!("launchd claims services and timers"),
    };
    check_name(kind, name)?;
    let mut d = Dictionary::new();
    d.insert("Label".into(), label(r).into());
    match r {
        Resource::Service(s) => service(s, &mut d)?,
        Resource::Timer(t) => timer(t, &mut d)?,
        _ => unreachable!(),
    }
    let mut out = Vec::new();
    Value::Dictionary(d).to_writer_xml(&mut out)?;
    Ok(String::from_utf8(out)?)
}

fn service(s: &Service, d: &mut Dictionary) -> Result<()> {
    d.insert("ProgramArguments".into(), program_arguments(s.exec())?);
    d.insert("RunAtLoad".into(), true.into());
    match s.restart {
        Restart::No => {}
        Restart::Always => {
            d.insert("KeepAlive".into(), true.into());
        }
        Restart::OnFailure => {
            let mut k = Dictionary::new();
            k.insert("SuccessfulExit".into(), false.into());
            d.insert("KeepAlive".into(), Value::Dictionary(k));
        }
    }
    environment(&s.env, d);
    for (k, v) in &s.launchd {
        d.insert(k.clone(), plist_value(v).with_context(|| format!("launchd.{k}"))?);
    }
    Ok(())
}

fn environment(vars: &BTreeMap<String, String>, d: &mut Dictionary) {
    if !vars.is_empty() {
        let mut env = Dictionary::new();
        for (k, v) in vars {
            env.insert(k.clone(), v.clone().into());
        }
        d.insert("EnvironmentVariables".into(), Value::Dictionary(env));
    }
}

/// A spec value as a plist value: Starlark has no dates or data, so neither does this.
fn plist_value(v: &serde_json::Value) -> Result<Value> {
    use serde_json::Value as J;
    Ok(match v {
        J::Bool(b) => Value::Boolean(*b),
        J::Number(n) => match (n.as_i64(), n.as_f64()) {
            (Some(i), _) => Value::Integer(i.into()),
            (None, Some(f)) => Value::Real(f),
            _ => anyhow::bail!("{n} is out of range"),
        },
        J::String(s) => Value::String(s.clone()),
        J::Array(items) => Value::Array(items.iter().map(plist_value).collect::<Result<_>>()?),
        J::Object(m) => {
            let mut d = Dictionary::new();
            for (k, v) in m {
                d.insert(k.clone(), plist_value(v)?);
            }
            Value::Dictionary(d)
        }
        J::Null => anyhow::bail!("null has no plist form"),
    })
}

fn timer(t: &Timer, d: &mut Dictionary) -> Result<()> {
    d.insert("ProgramArguments".into(), program_arguments(&t.exec)?);
    let schedule = Schedule::parse(&t.schedule).with_context(|| format!("timer {}", t.name))?;
    let intervals = schedule.calendar_intervals().with_context(|| format!("timer {}", t.name))?;
    let list = intervals
        .into_iter()
        .map(|i| {
            let mut e = Dictionary::new();
            for (key, v) in
                [("Minute", i.minute), ("Hour", i.hour), ("Day", i.day), ("Month", i.month), ("Weekday", i.weekday)]
            {
                if let Some(v) = v {
                    e.insert(key.into(), u64::from(v).into());
                }
            }
            Value::Dictionary(e)
        })
        .collect();
    d.insert("StartCalendarInterval".into(), Value::Array(list));
    environment(&t.env, d);
    Ok(())
}

fn program_arguments(exec: &str) -> Result<Value> {
    Ok(Value::Array(argv(exec, &BTreeMap::new())?.into_iter().map(Value::from).collect()))
}

#[cfg(test)]
mod tests;
