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

fn syncthing() -> Resource {
    resource(json!({
        "kind": "service", "name": "syncthing", "exec": "syncthing serve --no-browser", "user": true,
        "restart": "always", "after": ["network", "db", "graphical-session.target"],
        "env": {"A": "1", "B": "say \"hi\" 50%"}, "description": "Sync",
        "systemd": {"MemoryMax": "1G", "CPUQuota": ["50%"], "NoNewPrivileges": true,
                    "unit": {"StartLimitBurst": 5}, "install": {"WantedBy": "graphical-session.target"}}
    }))
}

#[test]
fn renders_a_user_service() {
    let units = units(&syncthing()).unwrap();
    assert_eq!(units.len(), 1);
    assert_eq!(units[0].path, "~/.config/systemd/user/fspec-syncthing.service");
    assert_eq!(
        units[0].content,
        r#"# Managed by fspec. Edits are overwritten on the next apply.

[Unit]
Description=Sync
After=network-online.target
After=fspec-db.service
After=graphical-session.target
StartLimitBurst=5

[Service]
ExecStart=/usr/bin/env syncthing serve --no-browser
Restart=always
Environment="A=1"
Environment="B=say \"hi\" 50%%"
CPUQuota=50%
MemoryMax=1G
NoNewPrivileges=yes

[Install]
WantedBy=graphical-session.target
"#
    );
}

#[test]
fn renders_a_system_service_with_absolute_exec() {
    let r = resource(json!({"kind": "service", "name": "web", "exec": "/usr/bin/web --port 80",
        "restart": "on_failure", "after": ["network"]}));
    let u = &units(&r).unwrap()[0];
    assert_eq!(u.path, "/etc/systemd/system/fspec-web.service");
    assert_eq!(
        u.content,
        "# Managed by fspec. Edits are overwritten on the next apply.\n\n[Unit]\nDescription=web\n\
         After=network-online.target\nWants=network-online.target\n\n[Service]\n\
         ExecStart=/usr/bin/web --port 80\nRestart=on-failure\n\n[Install]\nWantedBy=multi-user.target\n"
    );
}

#[test]
fn renders_a_timer_pair() {
    let r = resource(json!({"kind": "timer", "name": "backup", "exec": "/opt/backup.sh", "schedule": "30 2 * * 1-5"}));
    let us = units(&r).unwrap();
    assert_eq!(us.len(), 2);
    assert_eq!(us[0].path, "/etc/systemd/system/fspec-backup.service");
    assert_eq!(
        us[0].content,
        "# Managed by fspec. Edits are overwritten on the next apply.\n\n[Unit]\n\
         Description=backup (run by fspec-backup.timer)\n\n[Service]\nType=oneshot\nExecStart=/opt/backup.sh\n"
    );
    assert_eq!(us[1].path, "/etc/systemd/system/fspec-backup.timer");
    assert_eq!(
        us[1].content,
        "# Managed by fspec. Edits are overwritten on the next apply.\n\n[Unit]\nDescription=backup schedule\n\n\
         [Timer]\nOnCalendar=Mon..Fri *-*-* 02:30:00\nPersistent=true\n\n[Install]\nWantedBy=timers.target\n"
    );
}

#[test]
fn check_rejects_what_it_cannot_render() {
    let w = World::linux(Init::Systemd);
    let cx = w.cx(Scope::System);
    let bad_cron = resource(json!({"kind": "timer", "name": "t", "exec": "x", "schedule": "0 0 1 * 1"}));
    let err = SystemdBackend.check(&cx, &bad_cron).unwrap_err();
    assert!(format!("{err:#}").contains("both day-of-month and day-of-week"), "{err:#}");

    let bad_extra = resource(json!({"kind": "service", "name": "s", "exec": "x", "systemd": {"unit": "oops"}}));
    let err = SystemdBackend.check(&cx, &bad_extra).unwrap_err();
    assert!(format!("{err:#}").contains("`unit` must be a dict"), "{err:#}");

    let bad_name = resource(json!({"kind": "service", "name": "../x", "exec": "x"}));
    assert!(SystemdBackend.check(&cx, &bad_name).is_err());
}

#[test]
fn claims_only_systemd_hosts() {
    let r = syncthing();
    assert!(SystemdBackend.handles(&World::linux(Init::Systemd).cx(Scope::User), &r));
    assert!(!SystemdBackend.handles(&World::linux(Init::Runit).cx(Scope::User), &r));
    assert!(!SystemdBackend.handles(&World::darwin().cx(Scope::User), &r));
}

fn svc(exec: &str) -> Resource {
    resource(json!({"kind": "service", "name": "web", "exec": exec, "restart": "always"}))
}

#[test]
fn every_new_unit_is_written_before_any_starts() {
    let w = World::linux(Init::Systemd);
    let backends: &[&dyn Backend] = &[&SystemdBackend];
    let needs = resource(json!({"kind": "service", "name": "aura", "exec": "/bin/aura",
        "systemd": {"unit": {"Requires": "fspec-daemon.service"}}}));
    let daemon = resource(json!({"kind": "service", "name": "daemon", "exec": "/bin/daemon"}));
    apply_scope(&w.cx(Scope::System), backends, &[desired(needs), desired(daemon)]).unwrap();
    assert_eq!(
        w.runner.lines(),
        [
            "systemctl daemon-reload",
            "systemctl enable fspec-aura.service",
            "systemctl start fspec-aura.service",
            "systemctl enable fspec-daemon.service",
            "systemctl start fspec-daemon.service",
        ]
    );
}

#[test]
fn add_update_remove_command_sequences() {
    let w = World::linux(Init::Systemd);
    let backends: &[&dyn Backend] = &[&SystemdBackend];
    let unit = w.paths.root.join("etc/systemd/system/fspec-web.service");
    let timer_unit = w.paths.root.join("etc/systemd/system/fspec-job.timer");

    apply_scope(&w.cx(Scope::System), backends, &[desired(svc("/bin/web"))]).unwrap();
    assert!(fs::read_to_string(&unit).unwrap().contains("ExecStart=/bin/web\n"));
    assert_eq!(
        w.runner.lines(),
        ["systemctl daemon-reload", "systemctl enable fspec-web.service", "systemctl start fspec-web.service"]
    );

    w.runner.calls.lock().unwrap().clear();
    apply_scope(&w.cx(Scope::System), backends, &[desired(svc("/bin/web2"))]).unwrap();
    assert!(fs::read_to_string(&unit).unwrap().contains("ExecStart=/bin/web2\n"));
    assert_eq!(w.runner.lines(), ["systemctl daemon-reload", "systemctl restart fspec-web.service"]);

    w.runner.calls.lock().unwrap().clear();
    let job = resource(json!({"kind": "timer", "name": "job", "exec": "/bin/job", "schedule": "@daily"}));
    apply_scope(&w.cx(Scope::System), backends, &[desired(svc("/bin/web2")), desired(job)]).unwrap();
    assert_eq!(
        w.runner.lines(),
        ["systemctl daemon-reload", "systemctl enable fspec-job.timer", "systemctl start fspec-job.timer"]
    );
    assert!(timer_unit.exists());

    w.runner.calls.lock().unwrap().clear();
    apply_scope(&w.cx(Scope::System), backends, &[]).unwrap();
    assert_eq!(
        w.runner.lines(),
        [
            "systemctl stop fspec-web.service",
            "systemctl disable fspec-web.service",
            "systemctl daemon-reload",
            "systemctl reset-failed fspec-web.service",
            "systemctl stop fspec-job.timer",
            "systemctl disable fspec-job.timer",
            "systemctl daemon-reload",
            "systemctl reset-failed fspec-job.service fspec-job.timer",
        ]
    );
    assert!(!unit.exists() && !timer_unit.exists());
    assert!(!w.paths.root.join("etc/systemd/system/fspec-job.service").exists());
}

#[test]
fn a_unit_that_refuses_manual_start_is_only_enabled_and_disabled() {
    let w = World::linux(Init::Systemd);
    let backends: &[&dyn Backend] = &[&SystemdBackend];
    let app = |exec: &str| {
        resource(json!({"kind": "service", "name": "app", "exec": exec,
            "systemd": {"unit": {"RefuseManualStart": true, "RefuseManualStop": true}}}))
    };
    apply_scope(&w.cx(Scope::System), backends, &[desired(app("/bin/app"))]).unwrap();
    assert_eq!(w.runner.lines(), ["systemctl daemon-reload", "systemctl enable fspec-app.service"]);

    w.runner.calls.lock().unwrap().clear();
    apply_scope(&w.cx(Scope::System), backends, &[desired(app("/bin/app2"))]).unwrap();
    assert_eq!(w.runner.lines(), ["systemctl daemon-reload"]);

    w.runner.calls.lock().unwrap().clear();
    apply_scope(&w.cx(Scope::System), backends, &[]).unwrap();
    assert_eq!(
        w.runner.lines(),
        ["systemctl disable fspec-app.service", "systemctl daemon-reload", "systemctl reset-failed fspec-app.service"]
    );
}

#[test]
fn user_units_use_the_user_manager() {
    let w = World::linux(Init::Systemd);
    apply_scope(&w.cx(Scope::User), &[&SystemdBackend], &[desired(syncthing())]).unwrap();
    assert!(w.paths.home.join(".config/systemd/user/fspec-syncthing.service").exists());
    assert_eq!(
        w.runner.lines(),
        [
            "systemctl --user daemon-reload",
            "systemctl --user enable fspec-syncthing.service",
            "systemctl --user start fspec-syncthing.service",
        ]
    );
}

#[test]
fn failed_enable_rolls_back_files_and_replays_undo() {
    let w = World::linux(Init::Systemd);
    let backends: &[&dyn Backend] = &[&SystemdBackend];
    apply_scope(&w.cx(Scope::System), backends, &[desired(svc("/bin/web"))]).unwrap();
    w.runner.calls.lock().unwrap().clear();

    let job = resource(json!({"kind": "timer", "name": "job", "exec": "/bin/job", "schedule": "@daily"}));
    w.runner.respond("systemctl enable fspec-job.timer", Output { status: 1, ..Output::default() });
    let err = apply_scope(&w.cx(Scope::System), backends, &[desired(svc("/bin/web")), desired(job)]).unwrap_err();
    assert!(format!("{err:#}").contains("rolled back"), "{err:#}");
    assert!(!w.paths.root.join("etc/systemd/system/fspec-job.timer").exists());
    assert_eq!(
        w.runner.lines(),
        [
            "systemctl daemon-reload",
            "systemctl enable fspec-job.timer",
            "systemctl stop fspec-job.timer",
            "systemctl disable fspec-job.timer",
            "systemctl daemon-reload",
        ]
    );
    assert_eq!(State::load(&w.paths.system_state_dir().join("state.json")).unwrap().entries.len(), 1);
}

#[test]
fn a_unit_that_will_not_start_stays_owned_and_pending() {
    let w = World::linux(Init::Systemd);
    let backends: &[&dyn Backend] = &[&SystemdBackend];
    let job = resource(json!({"kind": "timer", "name": "job", "exec": "/bin/job", "schedule": "@daily"}));
    let failed = Output { status: 1, stderr: "Job for fspec-web.service failed.\n".into(), ..Output::default() };
    w.runner.respond("systemctl start fspec-web.service", failed.clone());
    let want = [desired(svc("/bin/web")), desired(job.clone())];
    apply_scope(&w.cx(Scope::System), backends, &want).unwrap();
    assert!(w.paths.root.join("etc/systemd/system/fspec-web.service").exists());
    assert!(w.runner.lines().contains(&"systemctl start fspec-job.timer".to_string()));
    assert!(w.runner.lines().contains(&"systemctl reset-failed fspec-web.service".to_string()));
    let state = || State::load(&w.paths.system_state_dir().join("state.json")).unwrap();
    assert_eq!(state().entries.len(), 2);

    // The next plan shows it, and a restart that fails again keeps it pending.
    let p = plan_scope(&w.cx(Scope::System), backends, &want, &state()).unwrap();
    assert!(matches!(&p.changes[..], [c] if c.key.id == "web"
        && matches!(&c.action, Action::Drift { notes } if notes[0].starts_with("not finished: `systemctl start"))));
    w.runner.respond("systemctl restart fspec-web.service", failed);
    apply_scope(&w.cx(Scope::System), backends, &want).unwrap();
    assert_eq!(plan_scope(&w.cx(Scope::System), backends, &want, &state()).unwrap().changes.len(), 1);

    w.runner.responses.lock().unwrap().clear();
    apply_scope(&w.cx(Scope::System), backends, &want).unwrap();
    assert!(plan_scope(&w.cx(Scope::System), backends, &want, &state()).unwrap().changes.is_empty());

    // Dropping the declaration of a pending unit removes it like any owned unit.
    w.runner.respond("systemctl start fspec-web.service", Output { status: 1, ..Output::default() });
    w.runner.respond("systemctl restart fspec-web.service", Output { status: 1, ..Output::default() });
    let edited = [desired(svc("/bin/web2")), desired(job.clone())];
    apply_scope(&w.cx(Scope::System), backends, &edited).unwrap();
    w.runner.calls.lock().unwrap().clear();
    apply_scope(&w.cx(Scope::System), backends, &[desired(job)]).unwrap();
    assert!(w.runner.lines().contains(&"systemctl disable fspec-web.service".to_string()));
    assert!(!w.paths.root.join("etc/systemd/system/fspec-web.service").exists());
}

#[test]
fn drift_reports_edited_and_missing_units() {
    let w = World::linux(Init::Systemd);
    let backends: &[&dyn Backend] = &[&SystemdBackend];
    let want = vec![desired(svc("/bin/web"))];
    apply_scope(&w.cx(Scope::System), backends, &want).unwrap();
    let state = || State::load(&w.paths.system_state_dir().join("state.json")).unwrap();
    assert!(plan_scope(&w.cx(Scope::System), backends, &want, &state()).unwrap().changes.is_empty());

    let unit = w.paths.root.join("etc/systemd/system/fspec-web.service");
    fs::write(&unit, "edited").unwrap();
    let p = plan_scope(&w.cx(Scope::System), backends, &want, &state()).unwrap();
    assert!(matches!(&p.changes[0].action, Action::Drift { notes }
        if notes == &["/etc/systemd/system/fspec-web.service was edited"]));

    fs::remove_file(&unit).unwrap();
    let p = plan_scope(&w.cx(Scope::System), backends, &want, &state()).unwrap();
    assert!(matches!(&p.changes[0].action, Action::Drift { notes } if notes[0].ends_with("is missing")));

    w.runner.calls.lock().unwrap().clear();
    apply_scope(&w.cx(Scope::System), backends, &want).unwrap();
    assert!(unit.exists());
    assert_eq!(w.runner.lines(), ["systemctl daemon-reload", "systemctl restart fspec-web.service"]);
}

#[test]
fn a_display_manager_boots_the_host_to_graphical_and_hands_the_old_default_back() {
    let w = World::linux(Init::Systemd);
    let backends: &[&dyn Backend] = &[&SystemdBackend];
    w.runner.respond("systemctl get-default", Output { stdout: "multi-user.target\n".into(), ..Default::default() });
    let greetd = resource(json!({"kind": "service", "name": "greetd", "display_manager": true}));
    apply_scope(&w.cx(Scope::System), backends, &[desired(greetd.clone())]).unwrap();
    assert_eq!(
        w.runner.lines(),
        ["systemctl enable --force greetd", "systemctl get-default", "systemctl set-default graphical.target"]
    );
    let state = State::load(&w.paths.system_state_dir().join("state.json")).unwrap();
    assert_eq!(state.entries.values().next().unwrap().record["previous_default"], "multi-user.target");

    w.runner.calls.lock().unwrap().clear();
    w.runner.responses.lock().unwrap().clear();
    w.runner.respond("systemctl get-default", Output { stdout: "graphical.target\n".into(), ..Default::default() });
    apply_scope(&w.cx(Scope::System), backends, &[]).unwrap();
    assert_eq!(
        w.runner.lines(),
        ["systemctl disable greetd", "systemctl get-default", "systemctl set-default multi-user.target"]
    );

    let user = resource(json!({"kind": "service", "name": "greetd", "display_manager": true, "user": true}));
    assert!(SystemdBackend.check(&w.cx(Scope::User), &user).is_err());
}

#[test]
fn rolling_back_a_forced_enable_puts_the_alias_it_took_back() {
    let w = World::linux(Init::Systemd);
    let lib = w.paths.root.join("usr/lib/systemd/system");
    fs::create_dir_all(&lib).unwrap();
    fs::write(lib.join("greetd.service"), "[Service]\nExecStart=greetd\n\n[Install]\nAlias=display-manager.service\n")
        .unwrap();
    let etc = w.paths.root.join("etc/systemd/system");
    fs::create_dir_all(&etc).unwrap();
    let alias = etc.join("display-manager.service");
    std::os::unix::fs::symlink("/usr/lib/systemd/system/display-manager-legacy.service", &alias).unwrap();

    let greetd = resource(json!({"kind": "service", "name": "greetd"}));
    let op = Op { key: greetd.key(), desired: Some(desired(greetd.clone())), current: None, drift: vec![] };
    let mut journal = fspec_core::journal::Journal::begin(&w.paths.system_state_dir(), 1).unwrap();
    let Resource::Service(s) = &greetd else { unreachable!() };
    crate::distro_unit::commit(&w.cx(Scope::System), Init::Systemd, &mut journal, &op, s).unwrap();
    fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink("/usr/lib/systemd/system/greetd.service", &alias).unwrap();

    w.runner.calls.lock().unwrap().clear();
    journal.rollback(&w.runner).unwrap();
    assert_eq!(w.runner.lines(), ["systemctl disable greetd", "systemctl daemon-reload"]);
    assert_eq!(fs::read_link(&alias).unwrap().to_str(), Some("/usr/lib/systemd/system/display-manager-legacy.service"));
}

#[test]
fn a_service_without_exec_enables_the_distro_unit() {
    let w = World::linux(Init::Systemd);
    let backends: &[&dyn Backend] = &[&SystemdBackend];
    let gdm = resource(json!({"kind": "service", "name": "gdm"}));
    apply_scope(&w.cx(Scope::System), backends, &[desired(gdm.clone())]).unwrap();
    assert_eq!(w.runner.lines(), ["systemctl enable --force gdm"]);
    assert!(!w.paths.root.join("etc/systemd/system/fspec-gdm.service").exists());

    w.runner.calls.lock().unwrap().clear();
    apply_scope(&w.cx(Scope::System), backends, &[]).unwrap();
    assert_eq!(w.runner.lines(), ["systemctl disable gdm"]);

    w.runner.calls.lock().unwrap().clear();
    let timer = resource(json!({"kind": "service", "name": "app.timer", "user": true}));
    apply_scope(&w.cx(Scope::User), backends, &[desired(timer)]).unwrap();
    assert_eq!(w.runner.lines(), ["systemctl --user enable --force app.timer", "systemctl --user start app.timer"]);

    let bad = resource(json!({"kind": "service", "name": "gdm", "restart": "always"}));
    let err = SystemdBackend.check(&w.cx(Scope::System), &bad).unwrap_err().to_string();
    assert!(err.contains("takes no restart"), "{err}");
}

#[test]
fn a_distro_swap_starts_on_a_running_system_and_retries_until_it_is_active() {
    let w = World::linux(Init::Systemd);
    let backends: &[&dyn Backend] = &[&SystemdBackend];
    let swap = vec![desired(resource(json!({"kind": "service", "name": "swap-swapfile.swap"})))];
    let cx = || w.cx(Scope::System);
    let state = || State::load(&w.paths.system_state_dir().join("state.json")).unwrap();

    w.runner.respond(
        "systemctl start swap-swapfile.swap",
        Output { status: 1, stderr: "Job failed.\n".into(), ..Default::default() },
    );
    apply_scope(&cx(), backends, &swap).unwrap();
    assert_eq!(
        w.runner.lines(),
        [
            "systemctl enable --force swap-swapfile.swap",
            "systemctl start swap-swapfile.swap",
            "systemctl reset-failed swap-swapfile.swap"
        ]
    );
    assert!(
        state().entries.values().all(|e| e.record.contains_key("pending")),
        "a unit that did not start is tried again"
    );

    w.runner.calls.lock().unwrap().clear();
    w.runner.responses.lock().unwrap().clear();
    apply_scope(&cx(), backends, &swap).unwrap();
    assert!(w.runner.lines().ends_with(&[
        "systemctl enable --force swap-swapfile.swap".to_string(),
        "systemctl start swap-swapfile.swap".to_string()
    ]));
    assert!(state().entries.values().all(|e| e.record.is_empty()));

    // Enabled by an earlier engine but never started: drift, and apply starts it.
    w.runner.respond("systemctl is-enabled", Output { stdout: "enabled\n".into(), ..Default::default() });
    w.runner.respond("systemctl is-active", Output { stdout: "inactive\n".into(), ..Default::default() });
    let p = plan_scope(&cx(), backends, &swap, &state()).unwrap();
    assert_eq!(p.changes[0].action, Action::Drift { notes: vec!["swap-swapfile.swap is inactive".into()] });

    w.runner.calls.lock().unwrap().clear();
    apply_scope(&cx(), backends, &[]).unwrap();
    assert_eq!(w.runner.lines(), ["systemctl disable swap-swapfile.swap"], "removal leaves it mounted");
}

#[test]
fn a_timer_env_goes_into_its_service() {
    let r = resource(json!({"kind": "timer", "name": "scan", "exec": "/opt/scan", "schedule": "@hourly",
        "env": {"VAULT": "/v"}}));
    assert!(units(&r).unwrap()[0].content.contains("Environment=\"VAULT=/v\"\n"));
}

/// A user `shell.service` whose main process (pid 42) runs `aaa-shell` while the store's
/// `current` link now points at `bbb-shell`. Returns the `/proc/42` directory.
fn stale_shell(w: &World) -> std::path::PathBuf {
    let store = w.paths.fspec_root().join("store");
    for id in ["aaa-shell", "bbb-shell"] {
        fs::create_dir_all(store.join(id).join("bin")).unwrap();
        fs::write(store.join(id).join("bin/shell"), "").unwrap();
    }
    let bin = w.paths.fspec_root().join("current/bin");
    fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink("../../store/bbb-shell/bin/shell", bin.join("shell")).unwrap();
    let proc = w.paths.root.join("proc/42");
    fs::create_dir_all(&proc).unwrap();
    std::os::unix::fs::symlink("/opt/fspec/store/aaa-shell/bin/shell", proc.join("exe")).unwrap();
    w.runner.respond(
        "systemctl --user show -p MainPID -p ExecStart shell.service",
        Output {
            stdout: "MainPID=42\nExecStart={ path=/opt/fspec/current/bin/shell ; argv[]=/opt/fspec/current/bin/shell ; ignore_errors=no }\n".into(),
            ..Default::default()
        },
    );
    proc
}

#[test]
fn a_service_running_a_replaced_store_package_restarts() {
    let w = World::linux(Init::Systemd);
    let backends: &[&dyn Backend] = &[&SystemdBackend];
    let proc = stale_shell(&w);
    let shell = vec![desired(resource(json!({"kind": "service", "name": "shell.service", "user": true})))];
    apply_scope(&w.cx(Scope::User), backends, &shell).unwrap();
    assert_eq!(w.runner.lines(), ["systemctl --user enable --force shell.service"]);

    let cx = w.cx(Scope::User);
    let state = State::load(&cx.state_dir().join("state.json")).unwrap();
    let p = plan_scope(&cx, backends, &shell, &state).unwrap();
    assert_eq!(
        p.changes[0].action,
        Action::Drift { notes: vec!["shell.service runs aaa-shell, the store now has bbb-shell".into()] }
    );
    w.runner.calls.lock().unwrap().clear();
    apply_scope(&cx, backends, &shell).unwrap();
    assert!(w.runner.lines().contains(&"systemctl --user restart shell.service".to_string()));

    // A package the same apply replaced is caught by the refresh after the commit.
    w.runner.calls.lock().unwrap().clear();
    let state = State::load(&cx.state_dir().join("state.json")).unwrap();
    let entries: Vec<Entry> = state.entries.values().cloned().collect();
    let mut journal = fspec_core::journal::Journal::begin(&cx.state_dir(), 99).unwrap();
    SystemdBackend.refresh(&cx, &entries, &mut journal).unwrap();
    assert_eq!(
        w.runner.lines(),
        ["systemctl --user show -p MainPID -p ExecStart shell.service", "systemctl --user restart shell.service"]
    );
    journal.finish().unwrap();

    // Up to date: nothing to restart.
    fs::remove_file(proc.join("exe")).unwrap();
    std::os::unix::fs::symlink("/opt/fspec/store/bbb-shell/bin/shell (deleted)", proc.join("exe")).unwrap();
    let state = State::load(&cx.state_dir().join("state.json")).unwrap();
    assert!(plan_scope(&cx, backends, &shell, &state).unwrap().changes.is_empty());

    // A bare command in ExecStart is not a store path.
    w.runner.responses.lock().unwrap().clear();
    w.runner.respond(
        "systemctl --user show -p MainPID -p ExecStart shell.service",
        Output {
            stdout: "MainPID=42\nExecStart={ path=shell ; argv[]=shell ; ignore_errors=no }\n".into(),
            ..Default::default()
        },
    );
    let state = State::load(&cx.state_dir().join("state.json")).unwrap();
    assert!(plan_scope(&cx, backends, &shell, &state).unwrap().changes.is_empty());
}

#[test]
fn a_refresh_restart_that_fails_leaves_the_service_pending() {
    let w = World::linux(Init::Systemd);
    stale_shell(&w);
    let failed = Output { status: 1, stderr: "Job for shell.service failed.\n".into(), ..Output::default() };
    w.runner.respond("systemctl --user restart shell.service", failed);
    let shell = resource(json!({"kind": "service", "name": "shell.service", "user": true}));
    let entries = [Entry { resource: shell.clone(), owner: None, record: Map::new() }];
    let cx = w.cx(Scope::User);
    let mut journal = fspec_core::journal::Journal::begin(&cx.state_dir(), 1).unwrap();
    let records = SystemdBackend.refresh(&cx, &entries, &mut journal).unwrap();
    assert!(
        records[&shell.key()]["pending"]
            .as_str()
            .unwrap()
            .starts_with("`systemctl --user restart shell.service` failed")
    );
    assert!(w.runner.lines().contains(&"systemctl --user reset-failed shell.service".to_string()));
    journal.finish().unwrap();
}

#[test]
fn one_systemctl_show_finds_the_stale_service_among_many() {
    let w = World::linux(Init::Systemd);
    let backends: &[&dyn Backend] = &[&SystemdBackend];
    let store = w.paths.fspec_root().join("store");
    for id in ["aaa-shell", "bbb-shell"] {
        fs::create_dir_all(store.join(id).join("bin")).unwrap();
        fs::write(store.join(id).join("bin/shell"), "").unwrap();
    }
    let bin = w.paths.fspec_root().join("current/bin");
    fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink("../../store/bbb-shell/bin/shell", bin.join("shell")).unwrap();
    let proc = w.paths.root.join("proc/42");
    fs::create_dir_all(&proc).unwrap();
    std::os::unix::fs::symlink("/opt/fspec/store/aaa-shell/bin/shell", proc.join("exe")).unwrap();
    let exec =
        "ExecStart={ path=/opt/fspec/current/bin/shell ; argv[]=/opt/fspec/current/bin/shell ; ignore_errors=no }";
    let idle = "MainPID=0\nExecStart=\n";
    w.runner.respond(
        "systemctl --user show -p MainPID -p ExecStart a.service b.service c.service",
        Output { stdout: format!("{idle}\nMainPID=42\n{exec}\n\n{idle}"), ..Default::default() },
    );
    let svc = |n: &str| desired(resource(json!({"kind": "service", "name": format!("{n}.service"), "user": true})));
    // A timer prints an empty block, so it is not asked about.
    let timer = desired(resource(json!({"kind": "service", "name": "d.timer", "user": true})));
    let want = vec![svc("a"), svc("b"), svc("c"), timer];
    apply_scope(&w.cx(Scope::User), backends, &want).unwrap();
    w.runner.calls.lock().unwrap().clear();

    let cx = w.cx(Scope::User);
    let state = State::load(&cx.state_dir().join("state.json")).unwrap();
    let p = plan_scope(&cx, backends, &want, &state).unwrap();
    assert_eq!(p.changes.len(), 1, "{p}");
    assert_eq!(p.changes[0].key.id, "b.service");
    let shows = w.runner.lines().into_iter().filter(|l| l.contains(" show ")).count();
    assert_eq!(shows, 1, "{:#?}", w.runner.lines());
}

#[test]
fn a_pending_distro_service_is_restarted_by_the_next_apply() {
    let w = World::linux(Init::Systemd);
    let asusd = resource(json!({"kind": "service", "name": "asusd"}));
    let mut record = Map::new();
    fspec_core::apply::pending(&mut record, "`systemctl restart asusd` failed");
    let current = Entry { resource: asusd.clone(), owner: None, record };
    let op = Op { key: asusd.key(), desired: Some(desired(asusd.clone())), current: Some(current), drift: vec![] };
    let cx = w.cx(Scope::System);
    let staging = tempfile::tempdir().unwrap();
    let mut journal = fspec_core::journal::Journal::begin(&cx.state_dir(), 1).unwrap();
    let records = SystemdBackend.commit(&cx, &[op], staging.path(), &mut journal).unwrap();
    journal.finish().unwrap();
    assert!(w.runner.lines().iter().any(|l| l.starts_with("systemctl restart asusd")), "{:#?}", w.runner.lines());
    assert!(!fspec_core::apply::is_pending(&records[&asusd.key()]));
}
