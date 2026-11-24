use std::fs;

use fspec_core::Scope;
use fspec_core::apply::{Backend, apply_scope, plan_scope};
use fspec_core::facts::Init;
use fspec_core::plan::Action;
use fspec_core::runner::Output;
use fspec_core::state::State;
use serde_json::json;

use super::*;
use crate::testutil::{World, desired, resource};

fn agent() -> Resource {
    resource(json!({
        "kind": "service", "name": "syncthing", "exec": "syncthing serve --home '/tmp/a b'", "user": true,
        "restart": "on_failure", "env": {"PATH": "/opt/homebrew/bin"}
    }))
}

#[test]
fn renders_a_service_plist() {
    assert_eq!(plist_path(&agent()), "~/Library/LaunchAgents/fspec.syncthing.plist");
    assert_eq!(
        render(&agent()).unwrap(),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>fspec.syncthing</string>
	<key>ProgramArguments</key>
	<array>
		<string>/usr/bin/env</string>
		<string>syncthing</string>
		<string>serve</string>
		<string>--home</string>
		<string>/tmp/a b</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<dict>
		<key>SuccessfulExit</key>
		<false/>
	</dict>
	<key>EnvironmentVariables</key>
	<dict>
		<key>PATH</key>
		<string>/opt/homebrew/bin</string>
	</dict>
</dict>
</plist>"#
    );
}

#[test]
fn shell_syntax_goes_through_sh() {
    let r =
        resource(json!({"kind": "service", "name": "x", "exec": "tail -f /var/log/a | logger", "restart": "always"}));
    let text = render(&r).unwrap();
    assert!(text.contains(
        "<string>/bin/sh</string>\n\t\t<string>-c</string>\n\t\t<string>tail -f /var/log/a | logger</string>"
    ));
    assert!(text.contains("<key>KeepAlive</key>\n\t<true/>"));
    assert_eq!(plist_path(&r), "/Library/LaunchDaemons/fspec.x.plist");

    let abs = resource(json!({"kind": "service", "name": "x", "exec": "/usr/bin/true"}));
    assert!(render(&abs).unwrap().contains("<array>\n\t\t<string>/usr/bin/true</string>\n\t</array>"));
}

#[test]
fn renders_a_timer_plist() {
    let r = resource(json!({"kind": "timer", "name": "backup", "exec": "/opt/backup.sh", "schedule": "0 9,17 * * 1"}));
    let text = render(&r).unwrap();
    assert!(!text.contains("RunAtLoad"));
    let expected = "<key>StartCalendarInterval</key>\n\t<array>\n\t\t<dict>\n\t\t\t<key>Minute</key>\n\t\t\t<integer>0</integer>\n\
        \t\t\t<key>Hour</key>\n\t\t\t<integer>9</integer>\n\t\t\t<key>Weekday</key>\n\t\t\t<integer>1</integer>\n\t\t</dict>\n\
        \t\t<dict>\n\t\t\t<key>Minute</key>\n\t\t\t<integer>0</integer>\n\t\t\t<key>Hour</key>\n\t\t\t<integer>17</integer>";
    assert!(text.contains(expected), "{text}");
}

#[test]
fn check_rejects_systemd_extras_and_bad_schedules() {
    let w = World::darwin();
    let cx = w.cx(Scope::System);
    let with_extras =
        resource(json!({"kind": "service", "name": "s", "exec": "x", "systemd": {"MemoryMax": "1G", "unit": {}}}));
    let err = LaunchdBackend.check(&cx, &with_extras).unwrap_err().to_string();
    assert_eq!(
        err,
        "service:s sets `systemd` (MemoryMax, unit), which only systemd supports, and this host uses launchd"
    );

    let runit = reject_init_extras(Init::Runit, &with_extras).unwrap_err().to_string();
    assert!(runit.contains("`systemd`") && runit.ends_with("this host uses runit"), "{runit}");
    assert!(reject_init_extras(Init::Runit, &agent()).is_ok());

    let range = resource(json!({"kind": "timer", "name": "t", "exec": "x", "schedule": "*/2 */2 * * *"}));
    let err = LaunchdBackend.check(&cx, &range).unwrap_err();
    assert!(format!("{err:#}").contains("calendar entries"), "{err:#}");
    let both = resource(json!({"kind": "timer", "name": "t", "exec": "x", "schedule": "0 0 1 * 1"}));
    assert!(LaunchdBackend.check(&cx, &both).is_err());
}

#[test]
fn daemon_command_sequences() {
    let w = World::darwin();
    let backends: &[&dyn Backend] = &[&LaunchdBackend];
    let plist = w.paths.root.join("Library/LaunchDaemons/fspec.web.plist");
    let p = plist.display();
    let svc = |exec: &str| resource(json!({"kind": "service", "name": "web", "exec": exec}));

    apply_scope(&w.cx(Scope::System), backends, &[desired(svc("/bin/web"))]).unwrap();
    assert!(plist.exists());
    assert_eq!(w.runner.lines(), [format!("launchctl bootstrap system {p}")]);

    w.runner.calls.lock().unwrap().clear();
    apply_scope(&w.cx(Scope::System), backends, &[desired(svc("/bin/web2"))]).unwrap();
    assert_eq!(
        w.runner.lines(),
        ["launchctl bootout system/fspec.web".to_string(), format!("launchctl bootstrap system {p}")]
    );

    w.runner.calls.lock().unwrap().clear();
    apply_scope(&w.cx(Scope::System), backends, &[]).unwrap();
    assert_eq!(w.runner.lines(), ["launchctl bootout system/fspec.web"]);
    assert!(!plist.exists());
}

#[test]
fn user_agents_use_the_gui_domain() {
    let w = World::darwin();
    w.runner.respond("id -u", Output { stdout: "501\n".into(), ..Output::default() });
    apply_scope(&w.cx(Scope::User), &[&LaunchdBackend], &[desired(agent())]).unwrap();
    let plist = w.paths.home.join("Library/LaunchAgents/fspec.syncthing.plist");
    assert!(plist.exists());
    assert_eq!(w.runner.lines(), ["id -u".to_string(), format!("launchctl bootstrap gui/501 {}", plist.display())]);
}

#[test]
fn failed_bootstrap_restores_the_old_plist_and_job() {
    let w = World::darwin();
    let backends: &[&dyn Backend] = &[&LaunchdBackend];
    let plist = w.paths.root.join("Library/LaunchDaemons/fspec.web.plist");
    let svc = |exec: &str| resource(json!({"kind": "service", "name": "web", "exec": exec}));
    apply_scope(&w.cx(Scope::System), backends, &[desired(svc("/bin/web"))]).unwrap();
    let old = fs::read_to_string(&plist).unwrap();
    w.runner.calls.lock().unwrap().clear();

    w.runner.respond("launchctl bootstrap", Output { status: 5, ..Output::default() });
    let err = apply_scope(&w.cx(Scope::System), backends, &[desired(svc("/bin/web2"))]).unwrap_err();
    assert!(format!("{err:#}").contains("apply failed"), "{err:#}");
    assert_eq!(fs::read_to_string(&plist).unwrap(), old);
    // Forward: bootout, bootstrap (fails). Undo: bootout the new job, bootstrap the restored plist.
    let lines = w.runner.lines();
    assert_eq!(lines.len(), 4, "{lines:?}");
    assert_eq!(lines[0], "launchctl bootout system/fspec.web");
    assert!(lines[1].starts_with("launchctl bootstrap system"));
    assert_eq!(lines[2], "launchctl bootout system/fspec.web");
    assert!(lines[3].starts_with("launchctl bootstrap system"));
}

#[test]
fn drift_reports_an_edited_plist() {
    let w = World::darwin();
    let backends: &[&dyn Backend] = &[&LaunchdBackend];
    let want = vec![desired(resource(json!({"kind": "service", "name": "web", "exec": "/bin/web"})))];
    apply_scope(&w.cx(Scope::System), backends, &want).unwrap();
    fs::write(w.paths.root.join("Library/LaunchDaemons/fspec.web.plist"), "x").unwrap();
    let state = State::load(&w.paths.system_state_dir().join("state.json")).unwrap();
    let p = plan_scope(&w.cx(Scope::System), backends, &want, &state).unwrap();
    assert_eq!(p.counts(), (0, 1, 0));
}

#[test]
fn launchd_keys_merge_over_the_defaults() {
    let idle = resource(json!({"kind": "service", "name": "usbflux", "exec": "/opt/x", "restart": "always",
        "launchd": {"RunAtLoad": false, "KeepAlive": false, "ThrottleInterval": 30, "WatchPaths": ["/Users/k/Vault"],
            "StandardOutPath": "/tmp/x.log", "ProcessType": "Interactive", "LimitLoadToSessionType": ["Aqua"]}}));
    let text = render(&idle).unwrap();
    assert!(text.contains("<key>RunAtLoad</key>\n\t<false/>"), "{text}");
    assert!(text.contains("<key>KeepAlive</key>\n\t<false/>"), "{text}");
    assert!(text.contains("<key>ThrottleInterval</key>\n\t<integer>30</integer>"));
    assert!(text.contains("<key>WatchPaths</key>\n\t<array>\n\t\t<string>/Users/k/Vault</string>"));
    assert!(text.contains("<key>LimitLoadToSessionType</key>\n\t<array>\n\t\t<string>Aqua</string>"));

    let err = crate::reject_init_extras(Init::Systemd, &idle).unwrap_err().to_string();
    assert!(err.contains("sets `launchd` (KeepAlive"), "{err}");
    let null = resource(json!({"kind": "service", "name": "n", "exec": "x", "launchd": {"Nice": null}}));
    assert!(render(&null).is_err());
}

#[test]
fn a_timer_carries_its_environment() {
    let r = resource(json!({"kind": "timer", "name": "scan", "exec": "/opt/scan", "schedule": "@hourly",
        "env": {"VAULT": "/Users/k/Vault"}}));
    let text = render(&r).unwrap();
    assert!(text.contains("<key>EnvironmentVariables</key>\n\t<dict>\n\t\t<key>VAULT</key>"), "{text}");
}

/// A user `fspec.t3` agent whose process (pid 42) runs `aaa-t3` while the store's `current` link
/// now points at `bbb-t3`.
fn stale_t3(w: &World) {
    let store = w.paths.fspec_root().join("store");
    for id in ["aaa-t3", "bbb-t3"] {
        fs::create_dir_all(store.join(id).join("bin")).unwrap();
        fs::write(store.join(id).join("bin/t3"), "").unwrap();
    }
    let bin = w.paths.fspec_root().join("current/bin");
    fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink("../../store/bbb-t3/bin/t3", bin.join("t3")).unwrap();
    w.runner.respond("id -u", Output { stdout: "501\n".into(), ..Output::default() });
    running_t3(w, "aaa-t3", RUNNING);
}

/// Answers lsof with `package` as the running executable, and `launchctl print` with `print`.
fn running_t3(w: &World, package: &str, print: &str) {
    let mut responses = w.runner.responses.lock().unwrap();
    responses.retain(|(cmd, _)| !cmd.starts_with("lsof") && !cmd.starts_with("launchctl print"));
    responses.push(("launchctl print gui/501/fspec.t3".into(), Output { stdout: print.into(), ..Output::default() }));
    responses.push((
        "lsof -a -p 42 -d txt -Fn".into(),
        Output {
            stdout: format!("p42\nftxt\nn/opt/fspec/store/{package}/bin/t3\nftxt\nn/usr/lib/dyld\n"),
            ..Output::default()
        },
    ));
}

const RUNNING: &str = "gui/501/fspec.t3 = {\n\tstate = running\n\tprogram = /opt/fspec/current/bin/t3\n\tpid = 42\n\
    \tendpoints = {\n\t\tpid = 7\n\t}\n}\n";

fn t3() -> Resource {
    resource(json!({"kind": "service", "name": "t3", "exec": "/opt/fspec/current/bin/t3 serve", "user": true}))
}

#[test]
fn an_agent_running_a_replaced_store_package_restarts() {
    let w = World::darwin();
    let backends: &[&dyn Backend] = &[&LaunchdBackend];
    stale_t3(&w);
    let want = vec![desired(t3())];
    let cx = w.cx(Scope::User);
    apply_scope(&cx, backends, &want).unwrap();

    let state = State::load(&cx.state_dir().join("state.json")).unwrap();
    let p = plan_scope(&cx, backends, &want, &state).unwrap();
    assert_eq!(
        p.changes[0].action,
        Action::Drift { notes: vec!["fspec.t3 runs aaa-t3, the store now has bbb-t3".into()] }
    );
    // The plist is unchanged, so the commit kickstarts the job instead of booting it out and in.
    w.runner.calls.lock().unwrap().clear();
    apply_scope(&cx, backends, &want).unwrap();
    let lines = w.runner.lines();
    assert!(lines.contains(&"launchctl kickstart -k gui/501/fspec.t3".to_string()), "{lines:#?}");
    assert!(!lines.iter().any(|l| l.starts_with("launchctl bootout")), "{lines:#?}");

    // A package the same apply replaced is caught by the refresh after the commit.
    w.runner.calls.lock().unwrap().clear();
    let state = State::load(&cx.state_dir().join("state.json")).unwrap();
    let entries: Vec<Entry> = state.entries.values().cloned().collect();
    let mut journal = fspec_core::journal::Journal::begin(&cx.state_dir(), 99).unwrap();
    assert!(LaunchdBackend.refresh(&cx, &entries, &mut journal).unwrap().is_empty());
    assert!(
        w.runner.lines().contains(&"launchctl kickstart -k gui/501/fspec.t3".to_string()),
        "{:#?}",
        w.runner.lines()
    );
    journal.finish().unwrap();

    // Up to date: nothing to restart.
    running_t3(&w, "bbb-t3", RUNNING);
    let state = State::load(&cx.state_dir().join("state.json")).unwrap();
    assert!(plan_scope(&cx, backends, &want, &state).unwrap().changes.is_empty());

    // A bare command runs through /usr/bin/env, which is not a store path.
    running_t3(&w, "aaa-t3", "\tprogram = /usr/bin/env\n\tpid = 42\n");
    assert!(plan_scope(&cx, backends, &want, &state).unwrap().changes.is_empty());

    // A job that is loaded but not running has no pid.
    running_t3(&w, "aaa-t3", "\tprogram = /opt/fspec/current/bin/t3\n\tstate = not running\n");
    assert!(plan_scope(&cx, backends, &want, &state).unwrap().changes.is_empty());
}

#[test]
fn a_refresh_restart_that_fails_leaves_the_agent_pending() {
    let w = World::darwin();
    stale_t3(&w);
    let failed = Output { status: 113, stderr: "Could not find service \"fspec.t3\"\n".into(), ..Output::default() };
    w.runner.respond("launchctl kickstart", failed);
    let entries = [Entry { resource: t3(), owner: None, record: serde_json::Map::new() }];
    let cx = w.cx(Scope::User);
    let mut journal = fspec_core::journal::Journal::begin(&cx.state_dir(), 1).unwrap();
    let records = LaunchdBackend.refresh(&cx, &entries, &mut journal).unwrap();
    assert_eq!(
        records[&t3().key()]["pending"],
        "`launchctl kickstart -k gui/501/fspec.t3` failed: Could not find service \"fspec.t3\""
    );
    journal.finish().unwrap();
}
