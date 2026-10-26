use std::fs;

use super::*;
use crate::fetch::MapFetch;
use crate::journal::Undo;
use crate::resource::{File, FileMode, System};
use crate::runner::{Cmd, RecordingRunner};

/// Writes `content` for each file, journaled; fails on a path containing `fail`.
struct FakeFiles;

impl Backend for FakeFiles {
    fn name(&self) -> &'static str {
        "fake-files"
    }
    fn handles(&self, _cx: &Cx, r: &Resource) -> bool {
        matches!(r, Resource::File(_))
    }
    fn phase(&self) -> Phase {
        Phase::Files
    }
    fn drift(&self, cx: &Cx, entry: &Entry) -> Result<Vec<String>> {
        let Resource::File(f) = &entry.resource else { return Ok(vec![]) };
        let on_disk = fs::read_to_string(cx.resolve(&f.path)?).unwrap_or_default();
        Ok(if Some(&Value::String(on_disk)) == f.content.as_ref() { vec![] } else { vec!["content changed".into()] })
    }
    fn commit(&self, cx: &Cx, ops: &[Op], _staging: &Path, journal: &mut Journal) -> Result<Records> {
        let mut records = Records::new();
        for op in ops {
            let Resource::File(f) = op.resource() else { unreachable!() };
            let path = cx.resolve(&f.path)?;
            journal.backup(&path)?;
            if op.is_removal() {
                remove_any(&path)?;
                continue;
            }
            if f.path.contains("fail") {
                bail!("refusing {}", f.path);
            }
            journal.record(Undo::Run { cmd: Cmd::new("echo").arg(format!("undo {}", f.path)) })?;
            fs::create_dir_all(path.parent().unwrap())?;
            fs::write(&path, f.content.as_ref().and_then(Value::as_str).unwrap_or(""))?;
            let mut rec = Map::new();
            rec.insert("written".into(), true.into());
            records.insert(op.key.clone(), rec);
        }
        Ok(records)
    }
}

fn file(path: &str, content: &str) -> Desired {
    let resource = Resource::File(File {
        path: path.into(),
        src: None,
        target: None,
        content: Some(content.into()),
        format: None,
        mode: FileMode::Managed,
        owns: Default::default(),
        on_conflict: Default::default(),
        on_drift: Default::default(),
        perms: None,
    });
    Desired { resource, owner: None }
}

struct World {
    _dir: tempfile::TempDir,
    paths: Paths,
    facts: Facts,
    lock: Lock,
    runner: RecordingRunner,
    fetch: MapFetch,
}

impl World {
    fn new() -> World {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().into(), dir.path().join("home/u"));
        let system = System::Distro(crate::resource::Distro {
            distro: "fedora".into(),
            init: crate::facts::Init::Systemd,
            packages: vec![],
            kernel_args: vec![],
        });
        let facts = Facts::declared("h", "x86_64-linux", Some(&system)).unwrap();
        World {
            _dir: dir,
            paths,
            facts,
            lock: Lock::default(),
            runner: RecordingRunner::default(),
            fetch: MapFetch::default(),
        }
    }

    fn cx(&self, scope: Scope) -> Cx<'_> {
        let generation = State::load(&state_dir(&self.paths, scope).join("state.json")).unwrap().generation + 1;
        Cx {
            paths: &self.paths,
            facts: &self.facts,
            spec_dir: &self.paths.root,
            lock: &self.lock,
            runner: &self.runner,
            fetch: &self.fetch,
            generation,
            scope,
            protect: &[],
            builders: &[],
            build_cache: None,
        }
    }

    fn state(&self, scope: Scope) -> State {
        State::load(&state_dir(&self.paths, scope).join("state.json")).unwrap()
    }
}

#[test]
fn apply_then_converged_then_drift_then_remove() {
    let w = World::new();
    let backends: &[&dyn Backend] = &[&FakeFiles];
    let want = vec![file("/etc/a", "A"), file("~/.b", "B")];

    let p = apply_scope(&w.cx(Scope::System), backends, &want).unwrap();
    assert_eq!(p.counts(), (1, 0, 0));
    apply_scope(&w.cx(Scope::User), backends, &want).unwrap();
    assert_eq!(fs::read_to_string(w.paths.root.join("etc/a")).unwrap(), "A");
    assert_eq!(fs::read_to_string(w.paths.home.join(".b")).unwrap(), "B");
    let state = w.state(Scope::System);
    assert_eq!(state.generation, 1);
    assert_eq!(state.entries[&want[0].resource.key()].record["written"], true);
    assert!(w.paths.system_state_dir().join("generations/1.json").exists());

    // Nothing to do: no new generation.
    assert!(apply_scope(&w.cx(Scope::System), backends, &want).unwrap().changes.is_empty());
    assert_eq!(w.state(Scope::System).generation, 1);

    fs::write(w.paths.root.join("etc/a"), "edited").unwrap();
    let p = plan_scope(&w.cx(Scope::System), backends, &want, &w.state(Scope::System)).unwrap();
    assert!(matches!(&p.changes[0].action, Action::Drift { notes } if notes == &["content changed"]));
    apply_scope(&w.cx(Scope::System), backends, &want).unwrap();
    assert_eq!(fs::read_to_string(w.paths.root.join("etc/a")).unwrap(), "A");

    apply_scope(&w.cx(Scope::System), backends, &[]).unwrap();
    assert!(!w.paths.root.join("etc/a").exists());
    assert!(w.state(Scope::System).entries.is_empty());
    assert_eq!(w.state(Scope::System).generation, 3);
}

#[test]
fn failure_rolls_back_everything() {
    let w = World::new();
    let backends: &[&dyn Backend] = &[&FakeFiles];
    apply_scope(&w.cx(Scope::System), backends, &[file("/etc/a", "A")]).unwrap();

    let err = apply_scope(&w.cx(Scope::System), backends, &[file("/etc/a", "A2"), file("/etc/fail", "x")]).unwrap_err();
    assert!(format!("{err:#}").contains("rolled back"), "{err:#}");
    assert_eq!(fs::read_to_string(w.paths.root.join("etc/a")).unwrap(), "A");
    assert!(!w.paths.root.join("etc/fail").exists());
    assert_eq!(w.state(Scope::System).generation, 1);
    assert!(w.runner.lines().contains(&"echo 'undo /etc/a'".to_string()));
    assert!(!w.paths.system_state_dir().join("journal.json").exists());
}

#[test]
fn unclaimed_resource_fails_the_plan() {
    let w = World::new();
    let pkg = Desired {
        resource: serde_json::from_value(
            serde_json::json!({"kind": "pkg", "name": "rg", "source": {"kind": "aqua", "name": "rg"}}),
        )
        .unwrap(),
        owner: None,
    };
    let p = plan_scope(&w.cx(Scope::System), &[&FakeFiles], std::slice::from_ref(&pkg), &State::default()).unwrap();
    assert_eq!(p.unsupported, ["no backend applies pkg:rg on h (fedora, systemd)"]);
    let err = apply_scope(&w.cx(Scope::System), &[&FakeFiles], &[pkg]).unwrap_err();
    assert!(err.to_string().contains("cannot apply"), "{err}");
}

/// Claims every package and records it, without touching the machine.
struct FakePkgs;

impl Backend for FakePkgs {
    fn name(&self) -> &'static str {
        "fake-pkgs"
    }
    fn handles(&self, _cx: &Cx, r: &Resource) -> bool {
        matches!(r, Resource::Pkg(_))
    }
    fn phase(&self) -> Phase {
        Phase::Packages
    }
    fn commit(&self, _cx: &Cx, ops: &[Op], _staging: &Path, _journal: &mut Journal) -> Result<Records> {
        Ok(ops.iter().filter(|o| !o.is_removal()).map(|o| (o.key.clone(), Map::new())).collect())
    }
}

fn pkg(name: &str, extra: serde_json::Value) -> Desired {
    let mut value = serde_json::json!({"kind": "pkg", "name": name, "source": {"kind": "aqua", "name": name}});
    value.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    Desired { resource: serde_json::from_value(value).unwrap(), owner: None }
}

fn data(w: &World, rel: &str) -> std::path::PathBuf {
    let path = w.paths.home.join(rel);
    fs::create_dir_all(&path).unwrap();
    fs::write(path.join("save"), "game").unwrap();
    path
}

const BOTH: &[&dyn Backend] = &[&FakePkgs, &FakeFiles];

#[test]
fn removing_a_package_trashes_its_data_unless_keep_data() {
    let w = World::new();
    let steam = data(&w, ".local/share/Steam");
    let kept = data(&w, ".local/share/Kept");
    let want = vec![
        pkg("steam", serde_json::json!({"data": ["~/.local/share/Steam"]})),
        pkg("kept", serde_json::json!({"data": ["~/.local/share/Kept"], "keep_data": true})),
    ];
    apply_scope(&w.cx(Scope::System), BOTH, &want).unwrap();

    let state = w.state(Scope::System);
    let plan = plan_scope(&w.cx(Scope::System), BOTH, &[], &state).unwrap();
    let steam_line = plan.changes.iter().find(|c| c.key.id == "steam").unwrap();
    assert!(steam_line.detail.contains("data to trash: ~/.local/share/Steam"), "{}", steam_line.detail);
    assert!(!plan.changes.iter().find(|c| c.key.id == "kept").unwrap().detail.contains("trash"));

    apply_scope(&w.cx(Scope::System), BOTH, &[]).unwrap();
    assert!(!steam.exists());
    assert!(kept.join("save").exists());
    let trashed = w.paths.home.join(".local/state/fspec/trash/system-2/steam/0-Steam/save");
    assert_eq!(fs::read_to_string(trashed).unwrap(), "game");
}

#[test]
fn a_failing_apply_puts_trashed_data_back() {
    let w = World::new();
    let steam = data(&w, ".local/share/Steam");
    let steam_pkg = pkg("steam", serde_json::json!({"data": ["~/.local/share/Steam"]}));
    apply_scope(&w.cx(Scope::System), BOTH, std::slice::from_ref(&steam_pkg)).unwrap();

    let err = apply_scope(&w.cx(Scope::System), BOTH, &[file("/etc/fail", "x")]).unwrap_err();
    assert!(format!("{err:#}").contains("rolled back"), "{err:#}");
    assert_eq!(fs::read_to_string(steam.join("save")).unwrap(), "game");
    assert_eq!(w.state(Scope::System).generation, 1);
    assert!(w.state(Scope::System).entries.contains_key(&steam_pkg.resource.key()));
}

#[test]
fn a_returning_package_gets_its_data_back() {
    let w = World::new();
    let steam = data(&w, ".local/share/Steam");
    let want = vec![pkg("steam", serde_json::json!({"data": ["~/.local/share/Steam"]}))];
    apply_scope(&w.cx(Scope::System), BOTH, &want).unwrap();
    apply_scope(&w.cx(Scope::System), BOTH, &[]).unwrap();
    assert!(!steam.exists());

    apply_scope(&w.cx(Scope::System), BOTH, &want).unwrap();
    assert_eq!(fs::read_to_string(steam.join("save")).unwrap(), "game");
}

#[test]
fn data_under_a_protected_path_fails_the_plan() {
    let w = World::new();
    data(&w, ".local/share/Steam");
    let want = [pkg("steam", serde_json::json!({"data": ["~/.local/share/Steam"]}))];
    let protect = ["~/.local/share".to_string()];
    let cx = Cx { protect: &protect, ..w.cx(Scope::System) };
    let err = plan_scope(&cx, BOTH, &want, &State::default()).unwrap_err();
    assert!(format!("{err:#}").contains("protected path"), "{err:#}");

    let other_home = ["/home".to_string()];
    let cx = Cx { protect: &other_home, ..w.cx(Scope::System) };
    assert!(cx.check_path(&w.paths.home.join("x")).is_ok(), "the applying user's own home stays usable");
    assert!(cx.check_path(&w.paths.system("/home/other/x")).is_err());
    assert!(cx.check_path(&w.paths.system("/home/u/../other/x")).is_err());
}

#[test]
fn new_entries_named_like_a_package_are_attributed_and_trashed_with_it() {
    let w = World::new();
    let want = vec![pkg("steam", serde_json::json!({}))];
    fs::create_dir_all(w.paths.home.join(".config/preexisting")).unwrap();
    apply_scope(&w.cx(Scope::System), BOTH, &want).unwrap();
    apply_scope(&w.cx(Scope::User), BOTH, &want).unwrap();
    assert!(w.state(Scope::User).data.is_empty(), "the first user apply only takes the baseline");

    for dir in [".config/Steam", ".cache/steam", ".config/unrelated", ".local/share/preexisting"] {
        fs::create_dir_all(w.paths.home.join(dir)).unwrap();
    }
    apply_scope(&w.cx(Scope::User), BOTH, &want).unwrap();
    let user = w.state(Scope::User);
    assert_eq!(user.data["steam"], ["~/.cache/steam", "~/.config/Steam"]);

    let orphans = crate::attribute::orphans(&w.paths, &w.state(Scope::System), &user);
    assert_eq!(orphans, ["~/.config/preexisting", "~/.config/unrelated", "~/.local/share/preexisting"]);

    apply_scope(&w.cx(Scope::System), BOTH, &[]).unwrap();
    assert!(!w.paths.home.join(".config/Steam").exists());
    assert!(!w.paths.home.join(".cache/steam").exists());
    assert!(w.paths.home.join(".config/unrelated").exists());

    // The next user apply drops claims of packages that are gone.
    apply_scope(&w.cx(Scope::User), BOTH, &[]).unwrap();
    assert!(w.state(Scope::User).data.is_empty());
}

#[test]
fn claiming_an_orphan_makes_it_the_packages_data() {
    let w = World::new();
    fs::create_dir_all(w.paths.home.join(".local/share/oddname")).unwrap();
    let mut user = State::default();
    let spec = crate::attribute::claim(&w.paths, &mut user, "~/.local/share/oddname", "steam").unwrap();
    assert_eq!(spec, "~/.local/share/oddname");
    assert!(crate::attribute::orphans(&w.paths, &State::default(), &user).is_empty());
    assert!(crate::attribute::claim(&w.paths, &mut user, "~/Documents", "steam").is_err());
}

/// Defers every package named `away`, as an unreachable repo would.
struct DeferringPkgs;

impl Backend for DeferringPkgs {
    fn name(&self) -> &'static str {
        "deferring-pkgs"
    }
    fn handles(&self, _cx: &Cx, r: &Resource) -> bool {
        matches!(r, Resource::Pkg(_))
    }
    fn phase(&self) -> Phase {
        Phase::Packages
    }
    fn commit(&self, _cx: &Cx, ops: &[Op], _staging: &Path, _journal: &mut Journal) -> Result<Records> {
        Ok(ops
            .iter()
            .filter(|o| !o.is_removal())
            .map(|o| (o.key.clone(), if o.key.id == "away" { deferred("repo:x is deferred") } else { Map::new() }))
            .collect())
    }
}

/// Runs `echo enable <name>` per service, so the test sees which ones were committed.
struct EchoServices;

impl Backend for EchoServices {
    fn name(&self) -> &'static str {
        "echo-services"
    }
    fn handles(&self, _cx: &Cx, r: &Resource) -> bool {
        matches!(r, Resource::Service(_))
    }
    fn phase(&self) -> Phase {
        Phase::Services
    }
    fn commit(&self, cx: &Cx, ops: &[Op], _staging: &Path, _journal: &mut Journal) -> Result<Records> {
        let mut records = Records::new();
        for op in ops.iter().filter(|o| !o.is_removal()) {
            cx.runner.check(&Cmd::new("echo").arg(format!("enable {}", op.key.id)))?;
            records.insert(op.key.clone(), Map::new());
        }
        Ok(records)
    }
}

#[test]
fn a_deferred_package_and_what_it_owns_are_left_out_and_retried() {
    let w = World::new();
    let backends: &[&dyn Backend] = &[&DeferringPkgs, &EchoServices];
    let service = |name: &str, owner: Option<&str>| Desired {
        resource: serde_json::from_value(serde_json::json!({"kind": "service", "name": name})).unwrap(),
        owner: owner.map(Into::into),
    };
    let want = [
        pkg("away", serde_json::json!({})),
        pkg("here", serde_json::json!({})),
        service("awayd", Some("away")),
        service("hered", Some("here")),
    ];
    apply_scope(&w.cx(Scope::System), backends, &want).unwrap();
    assert_eq!(w.runner.lines(), ["echo 'enable hered'"]);
    let state = w.state(Scope::System);
    let keys: Vec<String> = state.entries.keys().map(|k| k.to_string()).collect();
    assert_eq!(keys, ["pkg:here", "service:hered"]);

    let plan = plan_scope(&w.cx(Scope::System), backends, &want, &state).unwrap();
    let pending: Vec<String> = plan.changes.iter().map(|c| c.key.to_string()).collect();
    assert_eq!(pending, ["pkg:away", "service:awayd"]);
}

#[test]
fn a_second_apply_of_a_scope_is_refused_while_the_first_holds_it() {
    let dir = tempfile::tempdir().unwrap();
    let first = lock_scope(dir.path()).unwrap();
    let err = lock_scope(dir.path()).unwrap_err().to_string();
    assert!(err.contains(&format!("another apply (pid {})", std::process::id())), "{err}");
    drop(first);
    lock_scope(dir.path()).unwrap();
}
