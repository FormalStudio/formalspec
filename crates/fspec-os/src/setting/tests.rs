use fspec_core::Scope;
use fspec_core::apply::{Backend, apply_scope, plan_scope};
use fspec_core::plan::Action;
use fspec_core::runner::Output;
use fspec_core::state::State;
use serde_json::json;

use super::*;
use crate::testutil::{World, desired, resource};

fn setting(tool: &str, key: &str, value: serde_json::Value) -> Resource {
    resource(json!({"kind": "setting", "tool": tool, "key": key, "value": value}))
}

fn out(s: &str) -> Output {
    Output { stdout: s.into(), ..Output::default() }
}

const PMSET: &str = "Battery Power:\n Sleep On Power Button 1\n womp                 1\n sleep                10\n\
                     AC Power:\n Sleep On Power Button 1\n womp                 1\n sleep                0\n";

#[test]
fn check_takes_the_value_each_tool_writes() {
    let w = World::darwin();
    let cx = w.cx(Scope::System);
    assert!(SettingBackend.check(&cx, &setting("pmset", "sleep", json!(0))).is_ok());
    assert!(SettingBackend.check(&cx, &setting("pmset", "sleep", json!(-1))).is_err());
    assert!(SettingBackend.check(&cx, &setting("systemsetup", "restartfreeze", json!(true))).is_ok());
    assert!(SettingBackend.check(&cx, &setting("systemsetup", "restartfreeze", json!("on"))).is_err());
    assert!(SettingBackend.check(&cx, &setting("firewall", "global state", json!(false))).is_err());
}

#[test]
fn pmset_sets_every_power_source_and_restores_each_on_failure() {
    let w = World::darwin();
    let backends: &[&dyn Backend] = &[&SettingBackend];
    w.runner.respond("pmset -g custom", out(PMSET));
    w.runner.respond("systemsetup -getremotelogin", out("Remote Login: On\n"));
    w.runner.respond("systemsetup -getrestartfreeze", out("Restart After Freeze: Off\n"));
    w.runner.respond("systemsetup -setrestartfreeze on", Output { status: 1, ..Output::default() });
    let want = vec![
        desired(setting("pmset", "sleep", json!(0))),
        desired(setting("pmset", "womp", json!(1))),
        desired(setting("systemsetup", "remotelogin", json!(true))),
        desired(setting("systemsetup", "restartfreeze", json!(true))),
    ];
    let err = apply_scope(&w.cx(Scope::System), backends, &want).unwrap_err();
    assert!(format!("{err:#}").contains("rolled back"), "{err:#}");
    assert_eq!(
        w.runner.lines(),
        [
            "pmset -g custom",
            "pmset -a sleep 0",
            "pmset -g custom",
            "systemsetup -getremotelogin",
            "systemsetup -getrestartfreeze",
            "systemsetup -setrestartfreeze on",
            "systemsetup -setrestartfreeze off",
            "pmset -c sleep 0",
            "pmset -b sleep 10",
        ]
    );
}

#[test]
fn the_firewall_reads_its_state_and_removal_leaves_it() {
    let w = World::darwin();
    let backends: &[&dyn Backend] = &[&SettingBackend];
    w.runner.respond(&format!("{FIREWALL} --getglobalstate"), out("Firewall is enabled. (State = 1)\n"));
    w.runner.respond(&format!("{FIREWALL} --getstealthmode"), out("Firewall stealth mode is off\n"));
    let want = vec![
        desired(setting("firewall", "globalstate", json!(false))),
        desired(setting("firewall", "stealthmode", json!(false))),
    ];
    apply_scope(&w.cx(Scope::System), backends, &want).unwrap();
    assert_eq!(
        w.runner.lines(),
        [
            format!("{FIREWALL} --getglobalstate"),
            format!("{FIREWALL} --setglobalstate off"),
            format!("{FIREWALL} --getstealthmode"),
        ]
    );

    w.runner.calls.lock().unwrap().clear();
    apply_scope(&w.cx(Scope::System), backends, &[]).unwrap();
    assert!(w.runner.lines().is_empty(), "{:?}", w.runner.lines());
    assert!(State::load(&w.paths.system_state_dir().join("state.json")).unwrap().entries.is_empty());
}

#[test]
fn drift_names_a_power_source_that_differs() {
    let w = World::darwin();
    let backends: &[&dyn Backend] = &[&SettingBackend];
    let want = vec![desired(setting("pmset", "womp", json!(1)))];
    w.runner.respond("pmset -g custom", out(PMSET));
    apply_scope(&w.cx(Scope::System), backends, &want).unwrap();
    let plan = |w: &World| {
        let state = State::load(&w.paths.system_state_dir().join("state.json")).unwrap();
        plan_scope(&w.cx(Scope::System), backends, &want, &state).unwrap()
    };
    assert!(plan(&w).changes.is_empty());
    w.runner.responses.lock().unwrap().clear();
    w.runner.respond(
        "pmset -g custom",
        out(&PMSET.replace("womp                 1\n sleep                0", "womp 0\n sleep 0")),
    );
    assert!(matches!(&plan(&w).changes[0].action, Action::Drift { notes } if notes == &["value is 0"]));
}

#[test]
fn turning_remote_login_off_does_not_prompt() {
    let s = Setting { tool: SettingTool::Systemsetup, key: "remotelogin".into(), value: json!(false) };
    assert_eq!(write(&s, "-a", "off").to_string(), "systemsetup -setremotelogin -f off");
}

#[test]
fn spotlight_turns_indexing_off_on_every_volume() {
    let w = World::darwin();
    let cx = w.cx(Scope::System);
    assert!(SettingBackend.check(&cx, &setting("mdutil", "indexing", json!(false))).is_ok());
    assert!(SettingBackend.check(&cx, &setting("mdutil", "volume", json!(false))).is_err());
    let backends: &[&dyn Backend] = &[&SettingBackend];
    w.runner.respond(
        "mdutil -a -s",
        out("/:\n\tIndexing enabled. \n/Users/k/OrbStack:\n\tIndexing and searching disabled.\n"),
    );
    apply_scope(&cx, backends, &[desired(setting("mdutil", "indexing", json!(false)))]).unwrap();
    assert_eq!(w.runner.lines(), ["mdutil -a -s", "mdutil -a -i off"]);
    assert_eq!(indexing_state("/:\n\tIndexing disabled.\n/x:\n\tIndexing and searching disabled.\n"), Some("off"));
}
