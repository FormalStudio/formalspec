//! Applies the hosts of `examples/inits/spec.star` against a rooted temp dir with a recording
//! runner. The hosts' `distro` resource belongs to the system layer and the engine package every
//! Linux host carries to the package backends, so both are left out here.

use std::fs;
use std::path::Path;

use fspec_core::apply::{apply_scope, plan_scope, state_dir};
use fspec_core::engine;
use fspec_core::fetch::MapFetch;
use fspec_core::runner::{Output, RecordingRunner};
use fspec_core::{Backend, Cx, Desired, Facts, Lock, Paths, Resource, Scope, State};
use fspec_os::{DinitBackend, OpenrcBackend, RunitBackend, S6Backend, SystemdBackend};

struct Run {
    root: tempfile::TempDir,
    paths: Paths,
    runner: RecordingRunner,
}

fn apply(host: &str) -> Run {
    let spec_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/inits/spec.star");
    let spec = fspec_eval::evaluate(&spec_path).unwrap();
    let host = spec.host(host).unwrap();
    let facts = Facts::declared(&host.name, &host.platform, host.system.as_ref()).unwrap();
    let desired: Vec<Desired> = host
        .desired()
        .into_iter()
        .filter(|d| !matches!(d.resource, Resource::Distro(_)) && !engine::is_engine(&d.resource))
        .collect();

    let root = tempfile::tempdir().unwrap();
    let paths = Paths::new(root.path().into(), root.path().join("home/u"));
    let run = Run { root, paths, runner: RecordingRunner::default() };
    let (lock, fetch) = (Lock::default(), MapFetch::default());
    let backends: [&dyn Backend; 5] = [&RunitBackend, &OpenrcBackend, &DinitBackend, &S6Backend, &SystemdBackend];
    let cx = |scope, generation| Cx {
        paths: &run.paths,
        facts: &facts,
        spec_dir: spec_path.parent().unwrap(),
        lock: &lock,
        runner: &run.runner,
        fetch: &fetch,
        generation,
        scope,
        protect: &[],
        builders: &[],
    };
    for scope in [Scope::System, Scope::User] {
        let state = State::load(&state_dir(&run.paths, scope).join("state.json")).unwrap();
        apply_scope(&cx(scope, state.generation + 1), &backends, &desired).unwrap();
    }

    // A real crontab would now hold what `crontab -` was given; hand it back so drift sees it.
    let crontab = run.runner.calls.lock().unwrap().iter().rev().find_map(|c| c.stdin.clone());
    if let Some(text) = crontab {
        run.runner.respond("crontab -l", Output { stdout: text, ..Output::default() });
    }
    for scope in [Scope::System, Scope::User] {
        let state = State::load(&state_dir(&run.paths, scope).join("state.json")).unwrap();
        let again = plan_scope(&cx(scope, state.generation + 1), &backends, &desired, &state).unwrap();
        assert!(again.changes.is_empty(), "{scope:?}: {again}");
    }
    run
}

fn read(run: &Run, p: &str) -> String {
    fs::read_to_string(run.root.path().join(p)).unwrap()
}

fn expect_lines(run: &Run, expected: &[String]) {
    let lines = run.runner.lines();
    for e in expected {
        assert!(lines.iter().any(|l| l == e), "{e} missing from {lines:#?}");
    }
}

fn cron(run: &Run) {
    assert!(read(run, "etc/cron.d/fspec-backup").contains("30 2 * * 1-5 root /usr/local/bin/backup --all\n"));
    let stdin = run.runner.calls.lock().unwrap().iter().find_map(|c| c.stdin.clone()).unwrap();
    assert!(stdin.contains("# BEGIN fspec:notes\n@daily /usr/local/bin/sync-notes\n# END fspec:notes\n"), "{stdin}");
}

#[test]
fn void_host() {
    let run = apply("void");

    assert!(read(&run, "etc/sv/fspec-web/run").contains("export PORT=8080\nexec /usr/bin/webd --port 8080\n"));
    assert!(read(&run, "etc/sv/fspec-oneshot/finish").contains("sv down ."));
    assert!(read(&run, "etc/sv/fspec-worker/run").contains("sv check ../fspec-web"));
    assert_eq!(
        fs::read_link(run.root.path().join("etc/runit/runsvdir/default/fspec-web")).unwrap().to_str(),
        Some("/etc/sv/fspec-web")
    );
    assert!(read(&run, "etc/sv/fspec-runsvdir-u/run").contains("runsvdir /home/u/.local/service"));
    assert!(read(&run, "home/u/.local/service/fspec-syncthing/run").contains("exec syncthing serve --no-browser"));
    assert!(run.runner.lines().iter().all(|l| !l.starts_with("sv ")), "{:?}", run.runner.lines());
    cron(&run);
}

#[test]
fn openrc_host() {
    let run = apply("arch-openrc");
    let script = read(&run, "etc/init.d/fspec-web");
    assert!(script.contains("supervisor=supervise-daemon\n") && script.contains("\tneed net\n"), "{script}");
    expect_lines(
        &run,
        &[
            "rc-update add fspec-web default".into(),
            "rc-service fspec-web start".into(),
            "rc-update add fspec-oneshot default".into(),
        ],
    );
    cron(&run);
}

#[test]
fn dinit_host() {
    let run = apply("arch-dinit");
    assert!(
        read(&run, "etc/dinit.d/fspec-web").contains("command = /usr/bin/env 'PORT=8080' /usr/bin/webd --port 8080\n")
    );
    assert!(read(&run, "home/u/.config/dinit.d/fspec-syncthing").contains("restart = true\n"));
    expect_lines(
        &run,
        &[
            "dinitctl enable fspec-web".into(),
            "dinitctl start fspec-web".into(),
            "dinitctl --user enable fspec-syncthing".into(),
            "dinitctl --user start fspec-syncthing".into(),
        ],
    );
    cron(&run);
}

#[test]
fn s6_host() {
    let run = apply("arch-s6");
    assert_eq!(read(&run, "etc/s6/sv/fspec-web/type"), "longrun\n");
    assert!(run.root.path().join("etc/s6/sv/fspec/contents.d/fspec-worker").exists());
    let root = run.root.path().display();
    expect_lines(
        &run,
        &[
            format!("s6-rc-compile {root}/etc/s6/db/fspec-g1-a {root}/etc/s6/sv"),
            format!("s6-rc-update {root}/etc/s6/db/fspec-g1-a"),
            "s6-rc -u change fspec".into(),
        ],
    );
    cron(&run);
}
