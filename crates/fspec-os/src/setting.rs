//! macOS system settings behind a command: power management (`pmset`), `systemsetup`, the
//! application firewall (`socketfilterfw`) and Spotlight indexing (`mdutil`). All of them need root. Removing a `setting` stops
//! managing it and leaves the value: turning one back has no safe default (remote login off
//! cuts ssh), so the previous value is restored only when an apply fails.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Result, bail};
use fspec_core::Resource;
use fspec_core::apply::{Backend, Cx, Op, Phase, Records};
use fspec_core::facts::Os;
use fspec_core::journal::{Journal, Undo};
use fspec_core::resource::{Setting, SettingTool};
use fspec_core::runner::{Cmd, Runner};
use fspec_core::state::Entry;
use serde_json::{Map, Value};

const FIREWALL: &str = "/usr/libexec/ApplicationFirewall/socketfilterfw";

pub struct SettingBackend;

impl Backend for SettingBackend {
    fn name(&self) -> &'static str {
        "setting"
    }

    fn handles(&self, cx: &Cx, r: &Resource) -> bool {
        cx.facts.os == Os::Darwin && matches!(r, Resource::Setting(_))
    }

    fn phase(&self) -> Phase {
        Phase::Defaults
    }

    fn check(&self, _cx: &Cx, r: &Resource) -> Result<()> {
        let Resource::Setting(s) = r else { return Ok(()) };
        if s.key.is_empty() || !s.key.chars().all(|c| c.is_ascii_alphanumeric()) {
            bail!("setting:{} {} has an invalid key", s.tool.name(), s.key);
        }
        if s.tool == SettingTool::Mdutil && s.key != "indexing" {
            bail!("setting:mdutil {} has an invalid key: it takes only indexing", s.key);
        }
        word(s).map(drop)
    }

    fn drift(&self, cx: &Cx, entry: &Entry) -> Result<Vec<String>> {
        let Resource::Setting(s) = &entry.resource else { return Ok(vec![]) };
        let want = word(s)?;
        Ok(match read(cx.runner, s)? {
            None => vec![],
            Some(live) => live.values().filter(|v| **v != want).map(|v| format!("value is {v}")).take(1).collect(),
        })
    }

    fn commit(&self, cx: &Cx, ops: &[Op], _staging: &Path, journal: &mut Journal) -> Result<Records> {
        let mut records = Records::new();
        for op in ops {
            let Resource::Setting(s) = op.resource() else { unreachable!("setting claims settings") };
            if op.is_removal() {
                continue;
            }
            let want = word(s)?;
            let live = read(cx.runner, s)?.unwrap_or_default();
            if live.is_empty() || live.values().any(|v| *v != want) {
                for (source, old) in &live {
                    journal.record(Undo::Run { cmd: write(s, source, old) })?;
                }
                cx.runner.check(&write(s, "-a", &want))?;
            }
            records.insert(op.key.clone(), Map::new());
        }
        Ok(records)
    }
}

/// The value as the tool writes it: a number or path for pmset, `on`/`off` for the others.
fn word(s: &Setting) -> Result<String> {
    Ok(match (s.tool, &s.value) {
        (SettingTool::Pmset, Value::Number(n)) if n.is_u64() => n.to_string(),
        (SettingTool::Pmset, Value::String(v)) if !v.is_empty() && !v.contains(char::is_whitespace) => v.clone(),
        (SettingTool::Pmset, Value::Bool(b)) => u8::from(*b).to_string(),
        (SettingTool::Systemsetup | SettingTool::Firewall | SettingTool::Mdutil, Value::Bool(b)) => if *b { "on" } else { "off" }.into(),
        (tool, v) => bail!("setting:{} {} cannot be {v}", tool.name(), s.key),
    })
}

/// The live value per power source for pmset (`-b`, `-c`, `-u`), else under `-a`. `None` when
/// the tool printed nothing it could parse, as on a dry run.
fn read(runner: &dyn Runner, s: &Setting) -> Result<Option<BTreeMap<String, String>>> {
    let cmd = match s.tool {
        SettingTool::Pmset => Cmd::new("pmset").args(["-g", "custom"]),
        SettingTool::Systemsetup => Cmd::new("systemsetup").arg(format!("-get{}", s.key)),
        SettingTool::Firewall => Cmd::new(FIREWALL).arg(format!("--get{}", s.key)),
        SettingTool::Mdutil => Cmd::new("mdutil").args(["-a", "-s"]),
    };
    let out = runner.run(&cmd)?;
    if out.status != 0 || out.stdout.trim().is_empty() {
        return Ok(None);
    }
    let live = match s.tool {
        SettingTool::Pmset => pmset_values(&out.stdout, &s.key),
        SettingTool::Systemsetup => {
            on_off(out.stdout.rsplit(':').next().unwrap_or_default()).map(all).unwrap_or_default()
        }
        SettingTool::Firewall => firewall_state(&out.stdout).map(all).unwrap_or_default(),
        SettingTool::Mdutil => indexing_state(&out.stdout).map(all).unwrap_or_default(),
    };
    if live.is_empty() {
        bail!("setting:{} {}: could not read it from {:?}", s.tool.name(), s.key, out.stdout.trim());
    }
    Ok(Some(live))
}

fn all(v: &str) -> BTreeMap<String, String> {
    BTreeMap::from([("-a".to_string(), v.to_string())])
}

/// `pmset -g custom` lists each power source's settings as `<name> <value>` lines.
fn pmset_values(out: &str, key: &str) -> BTreeMap<String, String> {
    let mut source = None;
    let mut values = BTreeMap::new();
    for line in out.lines() {
        if !line.starts_with(char::is_whitespace) {
            source = match line.trim_end_matches(':') {
                "Battery Power" => Some("-b"),
                "AC Power" => Some("-c"),
                "UPS Power" => Some("-u"),
                _ => None,
            };
            continue;
        }
        if let (Some(src), Some((name, value))) = (source, line.trim().rsplit_once(char::is_whitespace))
            && name.trim() == key
        {
            values.insert(src.to_string(), value.to_string());
        }
    }
    values
}

fn on_off(s: &str) -> Option<&'static str> {
    match s.trim().to_ascii_lowercase().as_str() {
        "on" => Some("on"),
        "off" => Some("off"),
        _ => None,
    }
}

/// `Firewall is disabled. (State = 0)` for the global state, `... mode is off` for the rest.
fn firewall_state(out: &str) -> Option<&'static str> {
    let out = out.trim().trim_end_matches('.');
    if let Some((_, n)) = out.split_once("State = ") {
        return Some(if n.trim_end_matches(')').trim() == "0" { "off" } else { "on" });
    }
    let last = out.rsplit(' ').next()?;
    match last {
        "enabled" => Some("on"),
        "disabled" => Some("off"),
        w => on_off(w),
    }
}

/// `mdutil -a -s` prints `Indexing enabled.` or `Indexing disabled.` (or `Indexing and searching
/// disabled.` for a volume that opts out) under each volume. One enabled volume reads as on.
fn indexing_state(out: &str) -> Option<&'static str> {
    let states: Vec<_> = out.lines().map(str::trim).filter(|l| l.starts_with("Indexing")).collect();
    if states.is_empty() {
        return None;
    }
    Some(if states.iter().any(|l| l.starts_with("Indexing enabled")) { "on" } else { "off" })
}

/// `source` is a pmset power source flag; the other tools set one machine-wide value.
fn write(s: &Setting, source: &str, value: &str) -> Cmd {
    match s.tool {
        SettingTool::Pmset => Cmd::new("pmset").args([source, &s.key, value]),
        // Turning remote login off asks for confirmation unless forced.
        SettingTool::Systemsetup if s.key == "remotelogin" && value == "off" => {
            Cmd::new("systemsetup").args(["-setremotelogin", "-f", "off"])
        }
        SettingTool::Systemsetup => Cmd::new("systemsetup").args([format!("-set{}", s.key), value.into()]),
        SettingTool::Firewall => Cmd::new(FIREWALL).args([format!("--set{}", s.key), value.into()]),
        SettingTool::Mdutil => Cmd::new("mdutil").args(["-a", "-i", value]),
    }
}

#[cfg(test)]
mod tests;
