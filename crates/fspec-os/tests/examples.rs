//! Applies the hosts of `examples/os/spec.star` against a rooted temp dir with a recording runner.
//! The `fedora` host's `distro` resource and the engine package every Linux host carries belong to
//! the package backends, so they are left out here.

use std::fs;
use std::path::Path;

use fspec_core::apply::{apply_scope, plan_scope, state_dir};
use fspec_core::engine;
use fspec_core::fetch::MapFetch;
use fspec_core::runner::RecordingRunner;
use fspec_core::{Backend, Cx, Desired, Facts, Lock, Paths, Resource, Scope, State};
use fspec_os::{DefaultsBackend, LaunchdBackend, SystemdBackend, UsersBackend};

struct Run {
    root: tempfile::TempDir,
    paths: Paths,
    runner: RecordingRunner,
}

fn apply(host: &str) -> Run {
    let spec_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/os/spec.star");
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
    let backends: [&dyn Backend; 4] = [&SystemdBackend, &LaunchdBackend, &UsersBackend, &DefaultsBackend];
    for scope in [Scope::System, Scope::User] {
        let state = State::load(&state_dir(&run.paths, scope).join("state.json")).unwrap();
        let cx = Cx {
            paths: &run.paths,
            facts: &facts,
            spec_dir: spec_path.parent().unwrap(),
            lock: &lock,
            runner: &run.runner,
            fetch: &fetch,
            generation: state.generation + 1,
            scope,
            protect: &[],
            builders: &[],
            build_cache: None,
        };
        apply_scope(&cx, &backends, &desired).unwrap();

        let state = State::load(&state_dir(&run.paths, scope).join("state.json")).unwrap();
        let again = plan_scope(&Cx { generation: state.generation + 1, ..cx }, &backends, &desired, &state).unwrap();
        assert!(again.changes.is_empty(), "{scope:?}: {again}");
    }
    run
}

#[test]
fn fedora_host() {
    let run = apply("fedora");
    let sys = |p: &str| fs::read_to_string(run.root.path().join(p)).unwrap();

    let caddy = sys("etc/systemd/system/fspec-caddy.service");
    assert!(caddy.contains("After=docker.target\n"), "{caddy}");
    assert!(caddy.contains("StartLimitBurst=5\n") && caddy.contains("AmbientCapabilities=CAP_NET_BIND_SERVICE\n"));
    assert!(caddy.contains("MemoryMax=512M\n") && caddy.contains("NoNewPrivileges=yes\n"));
    assert!(sys("etc/systemd/system/fspec-backup.timer").contains("OnCalendar=Mon..Fri *-*-* 02:30:00\n"));
    assert!(sys("etc/systemd/system/fspec-tidy.timer").contains("OnCalendar=Sun *-*-* 00:00:00\n"));
    assert!(sys("home/u/.config/systemd/user/fspec-syncthing.service").contains("WantedBy=default.target\n"));

    let lines = run.runner.lines();
    for expected in [
        "getent passwd kyan",
        "useradd -m -u 1000 -s /usr/bin/fish kyan",
        "usermod -aG wheel,docker kyan",
        "systemctl start fspec-caddy.service",
        "systemctl start fspec-backup.timer",
        "systemctl start fspec-tidy.timer",
        "systemctl --user start fspec-syncthing.service",
    ] {
        assert!(lines.iter().any(|l| l == expected), "{expected} missing from {lines:#?}");
    }
}

#[test]
fn mac_host() {
    let run = apply("mac");
    let plist = |p: &str| fs::read_to_string(run.root.path().join(p)).unwrap();

    assert!(plist("Library/LaunchDaemons/fspec.tunnel.plist").contains("<string>/bin/sh</string>"));
    assert!(plist("home/u/Library/LaunchAgents/fspec.syncthing.plist").contains("<key>SuccessfulExit</key>"));
    assert!(plist("home/u/Library/LaunchAgents/fspec.backup.plist").contains("<key>StartCalendarInterval</key>"));

    let lines = run.runner.lines();
    for expected in [
        "chsh -s /usr/bin/fish kyan",
        "defaults write com.apple.dock autohide -bool true",
        "defaults write com.apple.dock tilesize -int 48",
        "defaults write NSGlobalDomain AppleShowAllExtensions -bool true",
        "defaults write com.apple.finder FXPreferredViewStyle -string Nlsv",
    ] {
        assert!(lines.iter().any(|l| l == expected), "{expected} missing from {lines:#?}");
    }
    assert!(lines.iter().any(|l| l.starts_with("launchctl bootstrap system ") && l.ends_with("fspec.tunnel.plist")));
    assert!(lines.iter().any(|l| l.starts_with("launchctl bootstrap gui/") && l.ends_with("fspec.backup.plist")));
}
