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
            "systemctl stop fspec-job.timer",
            "systemctl disable fspec-job.timer",
            "systemctl daemon-reload",
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
    assert_eq!(w.runner.lines(), ["systemctl disable fspec-app.service", "systemctl daemon-reload"]);
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
fn a_unit_that_will_not_start_is_deferred_and_the_rest_lands() {
    let w = World::linux(Init::Systemd);
    let backends: &[&dyn Backend] = &[&SystemdBackend];
    let job = resource(json!({"kind": "timer", "name": "job", "exec": "/bin/job", "schedule": "@daily"}));
    let failed = Output { status: 1, stderr: "Job for fspec-web.service failed.\n".into(), ..Output::default() };
    w.runner.respond("systemctl start fspec-web.service", failed);
    let want = [desired(svc("/bin/web")), desired(job)];
    apply_scope(&w.cx(Scope::System), backends, &want).unwrap();
    assert!(w.paths.root.join("etc/systemd/system/fspec-web.service").exists());
    assert!(w.runner.lines().contains(&"systemctl start fspec-job.timer".to_string()));
    let state = State::load(&w.paths.system_state_dir().join("state.json")).unwrap();
    assert_eq!(state.entries.keys().map(ToString::to_string).collect::<Vec<_>>(), ["timer:job"]);

    // The next apply starts it again.
    w.runner.responses.lock().unwrap().clear();
    w.runner.calls.lock().unwrap().clear();
    apply_scope(&w.cx(Scope::System), backends, &want).unwrap();
    assert!(w.runner.lines().contains(&"systemctl start fspec-web.service".to_string()));
    let state = State::load(&w.paths.system_state_dir().join("state.json")).unwrap();
    assert_eq!(state.entries.len(), 2);
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
    assert_eq!(w.runner.lines(), ["systemctl --user enable --force app.timer"]);

    let bad = resource(json!({"kind": "service", "name": "gdm", "restart": "always"}));
    let err = SystemdBackend.check(&w.cx(Scope::System), &bad).unwrap_err().to_string();
    assert!(err.contains("takes no restart"), "{err}");
}

#[test]
fn a_timer_env_goes_into_its_service() {
    let r = resource(json!({"kind": "timer", "name": "scan", "exec": "/opt/scan", "schedule": "@hourly",
        "env": {"VAULT": "/v"}}));
    assert!(units(&r).unwrap()[0].content.contains("Environment=\"VAULT=/v\"\n"));
}

#[test]
fn a_service_running_a_replaced_store_package_restarts() {
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
    w.runner.respond(
        "systemctl --user show -p MainPID -p ExecStart shell.service",
        Output {
            stdout: "MainPID=42\nExecStart={ path=/opt/fspec/current/bin/shell ; argv[]=/opt/fspec/current/bin/shell ; ignore_errors=no }\n".into(),
            ..Default::default()
        },
    );
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
