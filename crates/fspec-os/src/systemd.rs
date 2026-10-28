//! systemd units for `service` and `timer` resources.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use fspec_core::apply::{Backend, Cx, Op, Phase, Records};
use fspec_core::facts::Init;
use fspec_core::journal::{Journal, Undo, remove_any};
use fspec_core::resource::{Restart, Service, Timer};
use fspec_core::runner::{Cmd, Output};
use fspec_core::state::Entry;
use fspec_core::{Resource, Scope};
use serde_json::{Map, Value, json};

use crate::cron::Schedule;
use crate::{check_line, check_name, read_or_empty, write_file};

pub struct SystemdBackend;

/// A rendered unit file. `path` is a spec path, resolved against the rooted paths when used.
struct Unit {
    path: String,
    file: String,
    content: String,
}

impl Backend for SystemdBackend {
    fn name(&self) -> &'static str {
        "systemd"
    }

    fn handles(&self, cx: &Cx, r: &Resource) -> bool {
        cx.facts.init == Init::Systemd && matches!(r, Resource::Service(_) | Resource::Timer(_))
    }

    fn phase(&self) -> Phase {
        Phase::Services
    }

    fn check(&self, _cx: &Cx, r: &Resource) -> Result<()> {
        if let Some(s) = distro(r) {
            return crate::distro_unit::check(Init::Systemd, s);
        }
        crate::reject_init_extras(Init::Systemd, r)?;
        units(r).map(drop)
    }

    fn drift(&self, cx: &Cx, entry: &Entry) -> Result<Vec<String>> {
        Ok(self.drift_all(cx, std::slice::from_ref(entry))?.remove(0))
    }

    /// One `systemctl show` per manager finds the stale services among all of them.
    fn drift_all(&self, cx: &Cx, entries: &[Entry]) -> Result<Vec<Vec<String>>> {
        let rs: Vec<&Resource> = entries.iter().map(|e| &e.resource).collect();
        let stale = stale_all(cx, &rs)?;
        let mut notes = fspec_core::par::try_map(entries, |e| unit_drift(cx, &e.resource))?;
        for (n, s) in notes.iter_mut().zip(stale) {
            n.extend(s);
        }
        Ok(notes)
    }

    fn prepare(&self, _cx: &Cx, ops: &[Op], staging: &Path) -> Result<()> {
        for op in ops.iter().filter(|o| !o.is_removal() && distro(o.resource()).is_none()) {
            for u in units(op.resource())? {
                write_file(&staged(staging, op, &u), &u.content)?;
            }
        }
        Ok(())
    }

    fn commit(&self, cx: &Cx, ops: &[Op], staging: &Path, journal: &mut Journal) -> Result<Records> {
        let mut records = Records::new();
        let mut written = Vec::new();
        for op in ops {
            if let Some(s) = distro(op.resource()) {
                let rec = crate::distro_unit::commit(cx, Init::Systemd, journal, op, s)?;
                if !op.is_removal() {
                    let mut rec = rec;
                    // A service left pending by a failed restart is stopped, so it is not stale
                    // either, and only an explicit restart retries it.
                    // A swap or mount was just started again by the commit itself.
                    let retry = op.current.as_ref().is_some_and(|c| fspec_core::apply::is_pending(&c.record))
                        && !crate::distro_unit::starts(s)
                        && !refuses_manual(op.resource());
                    let failed = if retry {
                        restart(cx, journal, op.resource(), "it was pending")?
                    } else if op.current.is_some() {
                        restart_stale(cx, journal, op.resource())?
                    } else {
                        None
                    };
                    if let Some(why) = failed {
                        fspec_core::apply::pending(&mut rec, why);
                    }
                    records.insert(op.key.clone(), rec);
                }
                continue;
            }
            let user = is_user(op.resource());
            let main = main_unit(op.resource());
            let ctl = |args: &[&str]| ctl(user, args);
            let us = units(op.resource())?;
            // A unit that refuses manual start or stop only starts with what wants it, so apply
            // enables and disables it and leaves it running or not.
            let manual = !refuses_manual(op.resource());

            // Undo steps replay backwards: files are restored first, then the reload, then the
            // unit is brought back to its old state, so that step is recorded first.
            // systemctl refuses `--now` in a chroot but ignores start and stop there, so a root
            // built by `switch --distro` still applies.
            if op.is_removal() {
                if manual {
                    journal.record(Undo::Run { cmd: ctl(&["start", &main]) })?;
                }
                journal.record(Undo::Run { cmd: ctl(&["enable", &main]) })?;
            } else if op.current.is_some() && manual {
                journal.record(Undo::Run { cmd: ctl(&["restart", &main]) })?;
            }
            journal.record(Undo::Run { cmd: ctl(&["daemon-reload"]) })?;
            if op.is_removal() {
                for u in &us {
                    journal.backup(&cx.resolve(&u.path)?)?;
                }
                if manual {
                    cx.runner.run(&ctl(&["stop", &main]))?;
                }
                cx.runner.run(&ctl(&["disable", &main]))?;
                for u in &us {
                    remove_any(&cx.resolve(&u.path)?)?;
                }
                cx.runner.check(&ctl(&["daemon-reload"]))?;
                // A unit that failed (or exited non-zero on its stop) stays listed as failed, file
                // gone, until this clears it.
                let mut reset = vec!["reset-failed"];
                reset.extend(us.iter().map(|u| u.file.as_str()));
                cx.runner.run(&ctl(&reset))?;
                continue;
            }

            for u in &us {
                let dest = cx.resolve(&u.path)?;
                journal.backup(&dest)?;
                write_file(&dest, &fs::read_to_string(staged(staging, op, u))?)?;
            }
            let mut rec = Map::new();
            rec.insert("units".into(), json!(us.iter().map(|u| u.path.clone()).collect::<Vec<_>>()));
            records.insert(op.key.clone(), rec);
            written.push((op, user, main, manual));
        }
        // Every unit is on disk before any starts, so one that `Requires=` another from the same
        // apply finds it whatever order the two commit in.
        for user in [false, true] {
            if written.iter().any(|(_, u, _, _)| *u == user) {
                cx.runner.check(&ctl(user, &["daemon-reload"]))?;
            }
        }
        for (op, user, main, manual) in written {
            let ctl = |args: &[&str]| ctl(user, args);
            let start = if op.current.is_some() {
                manual.then(|| ctl(&["restart", &main]))
            } else {
                journal.record(Undo::Run { cmd: ctl(&["disable", &main]) })?;
                if manual {
                    journal.record(Undo::Run { cmd: ctl(&["stop", &main]) })?;
                }
                cx.runner.check(&ctl(&["enable", &main]))?;
                manual.then(|| ctl(&["start", &main]))
            };
            // A unit that is written and enabled but will not start is the unit's own problem (a
            // missing config, a port in use), so it is left pending rather than rolling back
            // every other change.
            let Some(start) = start else { continue };
            let out = cx.runner.run(&start)?;
            if out.status != 0 {
                let why = failed(cx, user, &main, &start, &out)?;
                fspec_core::apply::pending(records.entry(op.key.clone()).or_default(), why);
            }
        }
        Ok(records)
    }

    fn refresh(&self, cx: &Cx, entries: &[Entry], journal: &mut Journal) -> Result<Records> {
        let rs: Vec<&Resource> = entries.iter().map(|e| &e.resource).collect();
        let mut records = Records::new();
        for (e, why) in entries.iter().zip(stale_all(cx, &rs)?) {
            if let Some(why) = why
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

/// How the unit files or a distro unit's enablement diverged from the spec.
fn unit_drift(cx: &Cx, r: &Resource) -> Result<Vec<String>> {
    if let Some(s) = distro(r) {
        return crate::distro_unit::drift(cx, Init::Systemd, s);
    }
    let mut notes = Vec::new();
    for u in units(r)? {
        match read_or_empty(&cx.resolve(&u.path)?) {
            None => notes.push(format!("{} is missing", u.path)),
            Some(on_disk) if on_disk != u.content => notes.push(format!("{} was edited", u.path)),
            Some(_) => {}
        }
    }
    Ok(notes)
}

/// The unit fspec starts and stops for `r`: its own unit, or the distro's for a service
/// without `exec`.
fn unit_name(r: &Resource) -> String {
    match distro(r) {
        Some(s) => s.name.clone(),
        None => main_unit(r),
    }
}

/// Why a running service is stale: its `ExecStart` now resolves to another store package than
/// the one its main process was started from, as after an apply that moved the package's
/// `current` link. A unit that refuses manual restarts is left to the next session.
fn stale(cx: &Cx, r: &Resource) -> Result<Option<String>> {
    Ok(stale_all(cx, &[r])?.remove(0))
}

/// [`stale`] for many resources, with one `systemctl show` per manager. systemd prints each
/// unit's properties in the order the units were named, separated by a blank line.
fn stale_all(cx: &Cx, rs: &[&Resource]) -> Result<Vec<Option<String>>> {
    let mut out = vec![None; rs.len()];
    let store = cx.paths.fspec_root().join("store");
    if !store.is_dir() {
        return Ok(out);
    }
    for user in [false, true] {
        let asked: Vec<usize> = (0..rs.len())
            .filter(|&i| {
                matches!(rs[i], Resource::Service(_))
                    && !refuses_manual(rs[i])
                    && is_user(rs[i]) == user
                    && has_process(&unit_name(rs[i]))
            })
            .collect();
        if asked.is_empty() {
            continue;
        }
        let units: Vec<String> = asked.iter().map(|&i| unit_name(rs[i])).collect();
        let mut args = vec!["show", "-p", "MainPID", "-p", "ExecStart"];
        args.extend(units.iter().map(String::as_str));
        let shown = cx.runner.run(&ctl(user, &args))?.stdout;
        let blocks: Vec<&str> = shown.split("\n\n").collect();
        if blocks.len() != asked.len() {
            continue;
        }
        for ((&i, unit), block) in asked.iter().zip(&units).zip(blocks) {
            out[i] = stale_from(cx, &store, unit, block)?;
        }
    }
    Ok(out)
}

fn stale_from(cx: &Cx, store: &Path, unit: &str, shown: &str) -> Result<Option<String>> {
    let prop = |k: &str| shown.lines().find_map(|l| l.strip_prefix(k)?.strip_prefix('='));
    let Some(pid) = prop("MainPID").filter(|p| !matches!(*p, "" | "0")) else { return Ok(None) };
    // `{ path=/opt/fspec/current/bin/x ; argv[]=... }`. A distro unit may name a bare command
    // (`path=greetd`), which can never be a store path.
    let Some(exec) =
        prop("ExecStart").and_then(|e| e.split("path=").nth(1)?.split(" ;").next()).filter(|e| e.starts_with('/'))
    else {
        return Ok(None);
    };
    // The kernel reports a removed executable as `<path> (deleted)`; the package is the first
    // component under the store either way.
    let Ok(running) = fs::read_link(cx.resolve(&format!("/proc/{pid}/exe"))?) else { return Ok(None) };
    let Ok(now) = fs::canonicalize(cx.resolve(exec)?) else { return Ok(None) };
    let package = |p: &Path, store: &Path| {
        Some(p.strip_prefix(store).ok()?.components().next()?.as_os_str().to_string_lossy().into_owned())
    };
    let was = package(&cx.resolve(&running.to_string_lossy())?, store);
    let is = fs::canonicalize(store).ok().and_then(|store| package(&now, &store));
    Ok(match (was, is) {
        (Some(was), Some(is)) if was != is => Some(format!("{unit} runs {was}, the store now has {is}")),
        _ => None,
    })
}

/// Why `cmd` failed to start or restart `unit`, after clearing the unit's failed state: `Restart=`
/// retries can hit the start limit, which refuses the next apply's start until it is cleared.
pub(crate) fn failed(cx: &Cx, user: bool, unit: &str, cmd: &Cmd, out: &Output) -> Result<String> {
    let why = out.stderr.lines().chain(out.stdout.lines()).find(|l| !l.trim().is_empty()).unwrap_or("");
    let journalctl = if user { "journalctl --user -u" } else { "journalctl -u" };
    cx.runner.run(&ctl(user, &["reset-failed", unit]))?;
    Ok(format!("`{cmd}` failed: {}; see `{journalctl} {unit}`", why.trim()))
}

fn restart_stale(cx: &Cx, journal: &mut Journal, r: &Resource) -> Result<Option<String>> {
    match stale(cx, r)? {
        Some(why) => restart(cx, journal, r, &why),
        None => Ok(None),
    }
}

/// Restart a stale service. Returns why the restart failed, which leaves the service pending
/// like a failed start in commit instead of rolling back the apply.
fn restart(cx: &Cx, journal: &mut Journal, r: &Resource, why: &str) -> Result<Option<String>> {
    let (user, unit) = (is_user(r), unit_name(r));
    let restart = ctl(user, &["restart", &unit]);
    journal.record(Undo::Run { cmd: restart.clone() })?;
    let out = cx.runner.run(&restart)?;
    if out.status != 0 {
        return failed(cx, user, &unit, &restart, &out).map(Some);
    }
    println!("{}: restarted, {why}", r.key());
    Ok(None)
}

fn staged(staging: &Path, op: &Op, u: &Unit) -> std::path::PathBuf {
    staging.join("systemd").join(op.key.to_string().replace(':', "-")).join(&u.file)
}

fn distro(r: &Resource) -> Option<&fspec_core::resource::Service> {
    match r {
        Resource::Service(s) => crate::distro_unit::of(s),
        _ => None,
    }
}

fn ctl(user: bool, args: &[&str]) -> Cmd {
    Cmd::new("systemctl").args(user.then_some("--user")).args(args.iter().copied())
}

/// A `.timer`, `.path`, `.socket` or `.mount` has no `MainPID` or `ExecStart`, so `systemctl
/// show` prints an empty block for it, which the blank-line split cannot count.
fn has_process(unit: &str) -> bool {
    !unit.rsplit_once('.').is_some_and(|(_, suffix)| NO_PROCESS.contains(&suffix))
}

const NO_PROCESS: [&str; 10] =
    ["socket", "device", "mount", "automount", "swap", "target", "path", "timer", "slice", "scope"];

fn refuses_manual(r: &Resource) -> bool {
    let Resource::Service(s) = r else { return false };
    let unit = s.systemd.get("unit");
    ["RefuseManualStart", "RefuseManualStop"]
        .iter()
        .any(|k| unit.and_then(|u| u.get(k)).is_some_and(|v| v == &Value::Bool(true) || v == "yes"))
}

fn is_user(r: &Resource) -> bool {
    r.scope() == Scope::User
}

fn main_unit(r: &Resource) -> String {
    match r {
        Resource::Timer(t) => format!("fspec-{}.timer", t.name),
        Resource::Service(s) => format!("fspec-{}.service", s.name),
        _ => unreachable!("systemd claims services and timers"),
    }
}

fn units(r: &Resource) -> Result<Vec<Unit>> {
    match r {
        Resource::Service(s) => {
            check_name("service", &s.name)?;
            Ok(vec![unit(s.user, &format!("fspec-{}.service", s.name), render_service(s)?)])
        }
        Resource::Timer(t) => {
            check_name("timer", &t.name)?;
            Ok(vec![
                unit(t.user, &format!("fspec-{}.service", t.name), render_timer_service(t)?),
                unit(t.user, &format!("fspec-{}.timer", t.name), render_timer(t)?),
            ])
        }
        _ => unreachable!("systemd claims services and timers"),
    }
}

fn unit(user: bool, file: &str, content: String) -> Unit {
    let dir = if user { "~/.config/systemd/user" } else { "/etc/systemd/system" };
    Unit { path: format!("{dir}/{file}"), file: file.into(), content }
}

const HEADER: &str = "# Managed by fspec. Edits are overwritten on the next apply.\n";

/// Directives in file order. `set` replaces every earlier line for the same name.
#[derive(Default)]
struct Section(Vec<(String, String)>);

impl Section {
    fn push(&mut self, key: &str, value: impl Into<String>) {
        self.0.push((key.into(), value.into()));
    }

    fn set(&mut self, key: &str, values: Vec<String>) {
        self.0.retain(|(k, _)| k != key);
        self.0.extend(values.into_iter().map(|v| (key.to_string(), v)));
    }

    fn render(&self, title: &str, out: &mut String) {
        if self.0.is_empty() {
            return;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("[{title}]\n"));
        for (k, v) in &self.0 {
            out.push_str(&format!("{k}={v}\n"));
        }
    }
}

fn assemble(sections: [(&str, &Section); 3]) -> String {
    let mut out = HEADER.to_string();
    let mut body = String::new();
    for (title, s) in sections {
        s.render(title, &mut body);
    }
    if !body.is_empty() {
        out.push('\n');
    }
    out.push_str(&body);
    out
}

/// systemd does not search `PATH` for `ExecStart=`, so a bare command name goes through `env`.
/// `%` starts a specifier in unit files and is doubled; `$VAR` expansion is left to systemd.
fn exec_start(exec: &str) -> Result<String> {
    check_line("exec", exec)?;
    let exec = exec.trim();
    let Some(first) = exec.split_whitespace().next() else { bail!("exec is empty") };
    let exec = exec.replace('%', "%%");
    Ok(if first.starts_with('/') { exec } else { format!("/usr/bin/env {exec}") })
}

fn description(desc: Option<&str>, fallback: String) -> Result<String> {
    let d = desc.map(str::to_owned).unwrap_or(fallback);
    check_line("description", &d)?;
    Ok(d)
}

fn render_service(s: &Service) -> Result<String> {
    let mut unit = Section::default();
    let mut service = Section::default();
    let mut install = Section::default();

    unit.push("Description", description(s.description.as_deref(), s.name.clone())?);
    for dep in &s.after {
        check_line("after", dep)?;
        match dep.as_str() {
            "network" => {
                unit.push("After", "network-online.target");
                // User managers have no network-online.target to pull in.
                if !s.user {
                    unit.push("Wants", "network-online.target");
                }
            }
            d if d.ends_with(".target") => unit.push("After", d),
            d => {
                check_name("after", d)?;
                unit.push("After", format!("fspec-{d}.service"));
            }
        }
    }

    service.push("ExecStart", exec_start(s.exec())?);
    match s.restart {
        Restart::No => {}
        Restart::OnFailure => service.push("Restart", "on-failure"),
        Restart::Always => service.push("Restart", "always"),
    }
    for (k, v) in &s.env {
        if k.is_empty() || k.contains(['=', '\n', ' ']) || v.contains('\n') {
            bail!("env entry `{k}` of service {} cannot be written to a unit", s.name);
        }
        let v = v.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%");
        service.push("Environment", format!("\"{k}={v}\""));
    }
    install.push("WantedBy", if s.user { "default.target" } else { "multi-user.target" });

    apply_extras(&s.systemd, &mut unit, &mut service, &mut install)
        .with_context(|| format!("service {}: systemd = {{...}}", s.name))?;
    Ok(assemble([("Unit", &unit), ("Service", &service), ("Install", &install)]))
}

fn apply_extras(
    extras: &BTreeMap<String, Value>,
    unit: &mut Section,
    service: &mut Section,
    install: &mut Section,
) -> Result<()> {
    for (key, value) in extras {
        let target = match key.as_str() {
            "unit" => Some(&mut *unit),
            "service" => Some(&mut *service),
            "install" => Some(&mut *install),
            _ => None,
        };
        match target {
            Some(section) => {
                let Value::Object(map) = value else { bail!("`{key}` must be a dict of directives") };
                for (k, v) in map {
                    section.set(&directive(k)?, values(k, v)?);
                }
            }
            None => service.set(&directive(key)?, values(key, value)?),
        }
    }
    Ok(())
}

fn directive(name: &str) -> Result<String> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric()) {
        bail!("`{name}` is not a systemd directive name");
    }
    Ok(name.into())
}

/// A scalar is one line, a list repeats the directive. Booleans become `yes` and `no`.
fn values(key: &str, v: &Value) -> Result<Vec<String>> {
    let one = |v: &Value| -> Result<String> {
        let s = match v {
            Value::String(s) => s.clone(),
            Value::Bool(b) => if *b { "yes" } else { "no" }.into(),
            Value::Number(n) => n.to_string(),
            _ => bail!("`{key}` holds a value systemd cannot take (use a string, number, bool or list of those)"),
        };
        check_line(key, &s)?;
        Ok(s)
    };
    match v {
        Value::Array(items) => items.iter().map(one).collect(),
        v => Ok(vec![one(v)?]),
    }
}

fn render_timer_service(t: &Timer) -> Result<String> {
    let mut unit = Section::default();
    let mut service = Section::default();
    unit.push("Description", format!("{} (run by fspec-{}.timer)", t.name, t.name));
    service.push("Type", "oneshot");
    service.push("ExecStart", exec_start(&t.exec)?);
    for (k, v) in &t.env {
        service.push("Environment", format!("\"{k}={v}\""));
    }
    Ok(assemble([("Unit", &unit), ("Service", &service), ("Install", &Section::default())]))
}

fn render_timer(t: &Timer) -> Result<String> {
    let schedule = Schedule::parse(&t.schedule).with_context(|| format!("timer {}", t.name))?;
    let mut unit = Section::default();
    let mut timer = Section::default();
    let mut install = Section::default();
    unit.push("Description", format!("{} schedule", t.name));
    timer.push("OnCalendar", schedule.on_calendar());
    timer.push("Persistent", "true");
    install.push("WantedBy", "timers.target");
    Ok(assemble([("Unit", &unit), ("Timer", &timer), ("Install", &install)]))
}

#[cfg(test)]
mod tests;
