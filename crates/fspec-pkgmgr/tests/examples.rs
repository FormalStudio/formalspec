use std::path::PathBuf;

use fspec_core::apply::{Backend, Cx, apply_scope, plan_scope, state_dir};
use fspec_core::engine;
use fspec_core::fetch::MapFetch;
use fspec_core::resource::{Resource, Scope};
use fspec_core::runner::RecordingRunner;
use fspec_core::{Lock, Paths, State};
use fspec_pkgmgr::{BrewBackend, DnfBackend, FlatpakBackend};

/// Applies `examples/pkgmgr/<spec>.star` for `host` in `scope` against a recording runner.
/// The distro resource itself belongs to the system layer, which is not built yet, and the engine
/// package to the store.
fn run(spec: &str, host: &str, scope: Scope) -> (Vec<String>, usize) {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/pkgmgr");
    let spec = fspec_eval::evaluate(&dir.join(format!("{spec}.star"))).unwrap();
    let host = spec.host(host).unwrap();
    let want: Vec<_> = host
        .desired()
        .into_iter()
        .filter(|d| !matches!(d.resource, Resource::Distro(_)) && !engine::is_engine(&d.resource))
        .collect();
    let facts = fspec_core::Facts::declared(&host.name, &host.platform, host.system.as_ref()).unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::new(tmp.path().into(), tmp.path().join("home/u"));
    let lock = Lock::load(&dir.join("fspec.lock")).unwrap();
    let (runner, fetch) = (RecordingRunner::default(), MapFetch::default());
    let backends: [&dyn Backend; 3] = [&FlatpakBackend, &BrewBackend, &DnfBackend];
    let cx = |generation| Cx {
        paths: &paths,
        facts: &facts,
        spec_dir: &dir,
        lock: &lock,
        runner: &runner,
        fetch: &fetch,
        generation,
        scope,
        protect: &[],
        builders: &[],
    };
    apply_scope(&cx(1), &backends, &want).unwrap();
    let state = State::load(&state_dir(&paths, scope).join("state.json")).unwrap();
    let after = plan_scope(&cx(2), &backends, &want, &state).unwrap();
    assert!(after.changes.is_empty() && after.unsupported.is_empty(), "{after}");
    let applied = runner
        .lines()
        .into_iter()
        .filter(|l| !l.starts_with("rpm -q") && !l.starts_with("flatpak info") && !l.contains("list"))
        .collect();
    (applied, state.entries.len())
}

#[test]
fn brew_example() {
    let (lines, entries) = run("brew", "macbook", Scope::User);
    assert_eq!(entries, 3);
    assert_eq!(lines, ["brew install --cask aerospace", "brew install fd", "brew install ripgrep"]);
}

#[test]
fn fedora_example() {
    let (lines, entries) = run("fedora", "workstation", Scope::System);
    assert_eq!(entries, 4);
    let flatpak = "flatpak install -y --noninteractive --system flathub";
    assert_eq!(lines[0], "dnf install -y --setopt=max_parallel_downloads=10 jq ripgrep");
    assert_eq!(lines[1], "dnf install -y --setopt=max_parallel_downloads=10 htop");
    assert_eq!(lines[2], format!("{flatpak} org.mozilla.firefox"));
    assert!(lines[3].starts_with("flatpak update -y --noninteractive --system --commit=6f1c3a9b"), "{}", lines[3]);
    assert_eq!(lines[4], format!("{flatpak} com.valvesoftware.Steam"));
    assert_eq!(lines.len(), 5);
}
