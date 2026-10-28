//! `service("gdm")` without `exec`: a unit the distro ships, which fspec only enables at boot. A
//! service is not started, so enabling a display manager or sshd on a running system takes effect
//! at the next boot, and the same apply works inside a chroot. On systemd, a swap, mount,
//! automount, timer, path or socket unit is also started, since activating one never brings up a
//! daemon by itself; removing one only disables it, as unmounting a busy mount would fail the
//! apply. On systemd, `user = True` enables a user
//! unit, such as one an app's own installer would write. `display_manager = True` also makes the
//! host boot to graphical.target, since a distro's default can be multi-user.target, under which
//! a login screen wanted by graphical.target never starts.

use anyhow::{Result, bail};
use fspec_core::apply::{Cx, Op};
use fspec_core::facts::Init;
use fspec_core::journal::{Journal, Undo};
use fspec_core::resource::{Restart, Service};
use fspec_core::runner::Cmd;
use serde_json::{Map, Value};

use crate::{check_name, initsvc};

/// Runit's default runlevel. `/var/service` resolves to it once booted, but only through `/run`.
const RUNLEVEL: &str = "/etc/runit/runsvdir/default";

/// The service when it names a distro unit rather than declaring its own.
pub(crate) fn of(s: &Service) -> Option<&Service> {
    s.exec.is_none().then_some(s)
}

/// Unit types systemd activates without running a daemon, so apply starts them as well as
/// enabling them.
const STARTED: [&str; 6] = [".swap", ".mount", ".automount", ".timer", ".path", ".socket"];

pub(crate) fn starts(s: &Service) -> bool {
    STARTED.iter().any(|t| s.name.ends_with(t))
}

pub(crate) fn check(init: Init, s: &Service) -> Result<()> {
    check_name("service", &s.name)?;
    let extras = [
        ("user", s.user && init != Init::Systemd),
        ("restart", s.restart != Restart::No),
        ("after", !s.after.is_empty()),
        ("env", !s.env.is_empty()),
        ("description", s.description.is_some()),
        ("systemd", !s.systemd.is_empty()),
        ("display_manager", s.display_manager && init != Init::Systemd),
    ];
    let set: Vec<&str> = extras.iter().filter(|(_, on)| *on).map(|(k, _)| *k).collect();
    if !set.is_empty() {
        bail!("service:{} has no exec, so it enables the distro's own unit, which takes no {}", s.name, set.join(", "));
    }
    if s.display_manager && s.user {
        bail!("service:{} is a display manager, which is a system unit, not a user one", s.name);
    }
    if matches!(init, Init::S6 | Init::Launchd) {
        bail!("service:{} has no exec, and enabling a unit the system ships is not supported on {init}", s.name);
    }
    Ok(())
}

fn systemctl(verb: &str, s: &Service) -> Cmd {
    Cmd::new("systemctl").args(s.user.then_some("--user")).args(verb.split(' ')).arg(&s.name)
}

fn rc_update(verb: &str, s: &Service) -> Cmd {
    Cmd::new("rc-update").args([verb, s.name.as_str(), "default"])
}

fn dinitctl(verb: &str, s: &Service) -> Cmd {
    Cmd::new("dinitctl").args([verb, s.name.as_str()])
}

fn runit_link(s: &Service) -> String {
    format!("{RUNLEVEL}/{}", s.name)
}

pub(crate) fn drift(cx: &Cx, init: Init, s: &Service) -> Result<Vec<String>> {
    let enabled = match init {
        Init::Systemd => {
            let out = cx.runner.run(&systemctl("is-enabled", s))?.stdout;
            // A runner that only logs answers nothing, which says nothing about the unit.
            out.trim().is_empty() || matches!(out.trim(), "enabled" | "alias" | "static" | "indirect")
        }
        Init::Runit => cx.resolve(&runit_link(s))?.symlink_metadata().is_ok(),
        Init::Openrc => cx.resolve(&format!("/etc/runlevels/default/{}", s.name))?.symlink_metadata().is_ok(),
        Init::Dinit => cx.resolve(&format!("/etc/dinit.d/boot.d/{}", s.name))?.symlink_metadata().is_ok(),
        _ => true,
    };
    let mut notes = if enabled { vec![] } else { vec![format!("{} is not enabled", s.name)] };
    if init == Init::Systemd && starts(s) {
        let out = cx.runner.run(&systemctl("is-active", s))?.stdout;
        if !matches!(out.trim(), "" | "active" | "activating" | "reloading") {
            notes.push(format!("{} is {}", s.name, out.trim()));
        }
    }
    if init == Init::Systemd && s.display_manager {
        let default = cx.runner.run(&default_target("get-default"))?.stdout;
        if !matches!(default.trim(), "" | GRAPHICAL) {
            notes.push(format!("the host boots to {}, not {GRAPHICAL}", default.trim()));
        }
    }
    Ok(notes)
}

const GRAPHICAL: &str = "graphical.target";
const PREVIOUS: &str = "previous_default";

fn default_target(verb: &str) -> Cmd {
    Cmd::new("systemctl").arg(verb)
}

/// Boot to graphical.target while a display manager is declared, and back to the target it
/// replaced (kept in the record) once none is.
fn own_default(cx: &Cx, journal: &mut Journal, op: &Op, s: &Service) -> Result<Map<String, Value>> {
    let old = op.record();
    let previous = old.get(PREVIOUS).and_then(Value::as_str).map(str::to_string);
    let mut rec = Map::new();
    let want = s.display_manager && !op.is_removal();
    if !want && previous.is_none() {
        return Ok(rec);
    }
    let current = cx.runner.check(&default_target("get-default"))?.stdout.trim().to_string();
    let target = match (want, &previous) {
        (true, _) => {
            rec.insert(PREVIOUS.into(), previous.unwrap_or_else(|| current.clone()).into());
            GRAPHICAL.to_string()
        }
        (false, p) => p.clone().unwrap_or_default(),
    };
    if !current.is_empty() && current != target {
        journal.record(Undo::Run { cmd: default_target("set-default").arg(&current) })?;
        cx.runner.check(&default_target("set-default").arg(&target))?;
    }
    Ok(rec)
}

/// A unit that is enabled but will not start (a missing disk behind a mount) is left pending like
/// a declared service that fails to start, so the next apply tries again.
fn start(cx: &Cx, s: &Service, rec: &mut Map<String, Value>) -> Result<()> {
    let start = systemctl("start", s);
    let out = cx.runner.run(&start)?;
    if out.status != 0 {
        fspec_core::apply::pending(rec, crate::systemd::failed(cx, s.user, &s.name, &start, &out)?);
    }
    Ok(())
}

/// Where systemd looks for a unit file, in its order of precedence, and where `enable` links.
fn unit_dirs(user: bool) -> (&'static [&'static str], &'static str) {
    if user {
        (
            &["~/.config/systemd/user", "/etc/systemd/user", "/usr/local/lib/systemd/user", "/usr/lib/systemd/user"],
            "~/.config/systemd/user",
        )
    } else {
        (
            &["/etc/systemd/system", "/usr/local/lib/systemd/system", "/usr/lib/systemd/system", "/lib/systemd/system"],
            "/etc/systemd/system",
        )
    }
}

/// The links `enable --force` may take over from another unit: the `[Install] Alias=` names of
/// the unit file systemd would load.
fn alias_links(cx: &Cx, s: &Service) -> Result<Vec<std::path::PathBuf>> {
    let (dirs, links) = unit_dirs(s.user);
    // `systemctl enable greetd` means greetd.service.
    let typed = [".service", ".target", ".socket", ".device", ".slice", ".scope"].iter().chain(&STARTED);
    let name =
        if typed.into_iter().any(|t| s.name.ends_with(t)) { s.name.clone() } else { format!("{}.service", s.name) };
    let mut file = None;
    for dir in dirs {
        if let Some(text) = crate::read_or_empty(&cx.resolve(&format!("{dir}/{name}"))?) {
            file = Some(text);
            break;
        }
    }
    let mut section = "";
    let mut out = Vec::new();
    for line in file.as_deref().unwrap_or("").lines().map(str::trim) {
        if line.starts_with('[') {
            section = line;
        } else if section == "[Install]"
            && let Some(names) = line.strip_prefix("Alias").map(str::trim_start).and_then(|l| l.strip_prefix('='))
        {
            for name in names.split_whitespace().filter(|n| !n.contains('/')) {
                out.push(cx.resolve(&format!("{links}/{name}"))?);
            }
        }
    }
    Ok(out)
}

pub(crate) fn commit(cx: &Cx, init: Init, journal: &mut Journal, op: &Op, s: &Service) -> Result<Map<String, Value>> {
    let removal = op.is_removal();
    match init {
        Init::Systemd => {
            // `--force`: an alias the unit claims can already point at another unit, as
            // Tumbleweed's preset leaves display-manager.service on display-manager-legacy.service,
            // and the spec naming this one is what decides.
            let (verb, undo) = if removal { ("disable", "enable --force") } else { ("enable --force", "disable") };
            if !removal {
                // The rollback disables this unit before it puts back the alias links it took over.
                journal.record(Undo::Run {
                    cmd: Cmd::new("systemctl").args(s.user.then_some("--user")).arg("daemon-reload"),
                })?;
                for link in alias_links(cx, s)? {
                    journal.backup(&link)?;
                }
            }
            journal.record(Undo::Run { cmd: systemctl(undo, s) })?;
            cx.runner.check(&systemctl(verb, s))?;
            let mut rec = own_default(cx, journal, op, s)?;
            if !removal && starts(s) {
                start(cx, s, &mut rec)?;
            }
            return Ok(rec);
        }
        Init::Openrc => {
            let (verb, undo) = if removal { ("del", "add") } else { ("add", "del") };
            journal.record(Undo::Run { cmd: rc_update(undo, s) })?;
            cx.runner.check(&rc_update(verb, s))?;
        }
        Init::Dinit => {
            let (verb, undo) = if removal { ("disable", "enable") } else { ("enable", "disable") };
            journal.record(Undo::Run { cmd: dinitctl(undo, s) })?;
            cx.runner.check(&dinitctl(verb, s))?;
        }
        Init::Runit if removal => initsvc::unlink(cx, journal, &runit_link(s))?,
        Init::Runit => {
            let dir = format!("/etc/sv/{}", s.name);
            if !cx.resolve(&dir)?.is_dir() {
                bail!("service:{} has no exec, but {dir} does not exist; install the package that ships it", s.name);
            }
            initsvc::link(cx, journal, &runit_link(s), &dir)?;
        }
        other => bail!("enabling a unit the system ships is not supported on {other}"),
    }
    Ok(Map::new())
}
