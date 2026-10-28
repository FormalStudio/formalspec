use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use fspec_core::apply::{apply_scope, plan_scope, state_dir};
use fspec_core::fetch::MapFetch;
use fspec_core::plan::Action;
use fspec_core::resource::{Desired, OnConflict, Secret};
use fspec_core::runner::RecordingRunner;
use fspec_core::{Facts, Lock, Paths, Plan, Scope, State};
use serde_json::{Value, json};

use super::*;
use crate::adopt::{Outcome, adopt};
use crate::watch::{Watched, decide};

struct World {
    _dir: tempfile::TempDir,
    paths: Paths,
    spec: PathBuf,
    facts: Facts,
    lock: Lock,
    runner: RecordingRunner,
    fetch: MapFetch,
}

impl World {
    fn new() -> World {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().into(), dir.path().join("home/u"));
        let spec = dir.path().join("spec");
        fs::create_dir_all(spec.join("files")).unwrap();
        fs::create_dir_all(&paths.home).unwrap();
        World {
            paths,
            spec,
            facts: Facts::declared("h", "aarch64-darwin", None).unwrap(),
            lock: Lock::default(),
            runner: RecordingRunner::default(),
            fetch: MapFetch::default(),
            _dir: dir,
        }
    }

    fn cx(&self) -> Cx<'_> {
        Cx {
            paths: &self.paths,
            facts: &self.facts,
            spec_dir: &self.spec,
            lock: &self.lock,
            runner: &self.runner,
            fetch: &self.fetch,
            generation: self.state().generation + 1,
            scope: Scope::User,
            protect: &[],
            builders: &[],
            build_cache: None,
        }
    }

    fn state(&self) -> State {
        State::load(&state_dir(&self.paths, Scope::User).join("state.json")).unwrap()
    }

    fn apply(&self, want: &[Desired]) -> anyhow::Result<Plan> {
        apply_scope(&self.cx(), &[&FileBackend], want)
    }

    fn plan(&self, want: &[Desired]) -> Plan {
        plan_scope(&self.cx(), &[&FileBackend], want, &self.state()).unwrap()
    }

    fn at(&self, rel: &str) -> PathBuf {
        self.paths.home.join(rel)
    }

    fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.at(rel)).unwrap()
    }

    fn write_src(&self, rel: &str, text: &str) {
        fs::write(self.spec.join(rel), text).unwrap();
    }
}

fn file(path: &str) -> File {
    File {
        path: path.into(),
        src: None,
        target: None,
        content: None,
        format: None,
        mode: FileMode::Managed,
        owns: Default::default(),
        on_conflict: Default::default(),
        on_drift: Default::default(),
        perms: None,
    }
}

fn text(path: &str, content: &str) -> File {
    File { content: Some(content.into()), ..file(path) }
}

fn want(files: impl IntoIterator<Item = File>) -> Vec<Desired> {
    files.into_iter().map(|f| Desired { resource: Resource::File(f), owner: None }).collect()
}

fn drift_notes_of(p: &Plan) -> Vec<String> {
    p.changes
        .iter()
        .flat_map(|c| match &c.action {
            Action::Drift { notes } => notes.clone(),
            _ => vec![],
        })
        .collect()
}

#[test]
fn managed_writes_records_hash_and_reports_drift() {
    let w = World::new();
    let f = text("~/.config/a.txt", "hello\n");
    w.apply(&want([f.clone()])).unwrap();
    assert_eq!(w.read(".config/a.txt"), "hello\n");
    let rec = &w.state().entries[&Resource::File(f.clone()).key()].record;
    assert_eq!(rec["sha256"], sha256(b"hello\n"));
    assert!(w.plan(&want([f.clone()])).changes.is_empty());

    fs::write(w.at(".config/a.txt"), "edited").unwrap();
    assert_eq!(drift_notes_of(&w.plan(&want([f.clone()]))), ["content changed"]);
    w.apply(&want([f.clone()])).unwrap();
    assert_eq!(w.read(".config/a.txt"), "hello\n");
    assert!(w.plan(&want([f])).changes.is_empty());
}

#[test]
fn managed_src_and_perms_and_source_change() {
    let w = World::new();
    w.write_src("files/run.sh", "#!/bin/sh\n");
    let f = File { src: Some("files/run.sh".into()), perms: Some("0750".into()), ..file("~/bin/run.sh") };
    w.apply(&want([f.clone()])).unwrap();
    assert_eq!(fs::metadata(w.at("bin/run.sh")).unwrap().permissions().mode() & 0o7777, 0o750);

    w.write_src("files/run.sh", "#!/bin/sh\necho v2\n");
    assert_eq!(drift_notes_of(&w.plan(&want([f.clone()]))), ["source changed"]);
    w.apply(&want([f])).unwrap();
    assert_eq!(w.read("bin/run.sh"), "#!/bin/sh\necho v2\n");
}

#[test]
fn a_src_moved_with_the_same_bytes_is_not_rewritten() {
    let w = World::new();
    w.write_src("files/a.conf", "same\n");
    w.write_src("b.conf", "same\n");
    let f = File { src: Some("files/a.conf".into()), ..file("~/a.conf") };
    w.apply(&want([f.clone()])).unwrap();
    let moved = File { src: Some("b.conf".into()), ..f.clone() };
    // The old source is gone from the new spec tree, as when a spec moves its files.
    fs::remove_file(w.spec.join("files/a.conf")).unwrap();
    assert!(w.plan(&want([moved.clone()])).changes.is_empty());
    let generation = w.state().generation;
    w.apply(&want([moved.clone()])).unwrap();
    let state = w.state();
    assert_eq!(state.generation, generation);
    assert_eq!(state.entries[&Resource::File(moved.clone()).key()].resource, Resource::File(moved.clone()));

    w.write_src("b.conf", "other\n");
    assert!(matches!(&w.plan(&want([moved])).changes[0].action, Action::Drift { .. }));
}

#[test]
fn on_drift_fail_points_at_adopt() {
    let w = World::new();
    let f = File { on_drift: OnDrift::Fail, ..text("~/.x", "one") };
    w.apply(&want([f.clone()])).unwrap();
    fs::write(w.at(".x"), "app wrote this").unwrap();
    let err = format!("{:#}", w.apply(&want([f])).unwrap_err());
    assert!(err.contains("fspec adopt ~/.x"), "{err}");
    assert_eq!(w.read(".x"), "app wrote this");
}

#[test]
fn structured_content_formats() {
    let v = json!({"name": "x", "n": 3, "on": true, "list": ["a", "b"], "sub": {"k": "v"}});
    let toml = render::serialize(&v, Format::Toml).unwrap();
    assert!(toml.contains("name = \"x\"") && toml.contains("[sub]"), "{toml}");
    assert_eq!(render::parse(&toml, Format::Toml).unwrap(), v);
    let yaml = render::serialize(&v, Format::Yaml).unwrap();
    assert_eq!(render::parse(&yaml, Format::Yaml).unwrap(), v);
    assert_eq!(render::parse(&render::serialize(&v, Format::Json).unwrap(), Format::Json).unwrap(), v);

    let ini = render::serialize(&json!({"top": "1", "main": {"a": "b", "c": true}}), Format::Ini).unwrap();
    assert_eq!(ini, "top = 1\n\n[main]\na = b\nc = true\n");
    assert!(render::serialize(&json!({"a": {"b": {"c": 1}}}), Format::Ini).is_err());

    let plist = render::serialize(&json!({"Label": "x", "Keep": true, "N": [1, 2]}), Format::Plist).unwrap();
    assert!(plist.contains("<key>Label</key>") && plist.contains("<true/>"), "{plist}");

    let lua = render::serialize(&json!({"a": 1, "end": "q\"", "list": [1, "x"], "my key": {}}), Format::Lua).unwrap();
    assert_eq!(
        lua,
        "return {\n  a = 1,\n  [\"end\"] = \"q\\\"\",\n  list = {\n    1,\n    \"x\",\n  },\n  [\"my key\"] = {},\n}\n"
    );
}

#[test]
fn format_is_inferred_from_the_extension_and_unknown_fails_at_check() {
    let w = World::new();
    let f = File { content: Some(json!({"a": 1})), ..file("~/.config/a.toml") };
    w.apply(&want([f])).unwrap();
    assert_eq!(w.read(".config/a.toml"), "a = 1\n");

    let bad = File { content: Some(json!({"a": 1})), ..file("~/.config/a.conf") };
    let err = w.apply(&want([bad.clone()])).unwrap_err();
    assert!(format!("{err:#}").contains("cannot infer a format"), "{err:#}");
    let forced = File { format: Some(Format::Json), ..bad };
    w.apply(&want([forced])).unwrap();
    assert!(w.read(".config/a.conf").contains("\"a\": 1"));
}

#[test]
fn check_rejects_bad_declarations() {
    let w = World::new();
    let cases = [
        File { src: Some("files/x".into()), ..text("~/a", "both") },
        file("~/neither"),
        File { src: Some("files/missing".into()), ..file("~/a") },
        File { mode: FileMode::Link, ..file("~/a") },
        File { mode: FileMode::Patch, ..file("~/a.conf") },
        File { perms: Some("rw".into()), ..text("~/a", "x") },
    ];
    for f in cases {
        assert!(w.apply(&want([f.clone()])).is_err(), "{f:?}");
    }
}

#[test]
fn link_points_into_the_repo_and_copies_back_a_replaced_symlink() {
    let w = World::new();
    w.write_src("files/rc", "repo version\n");
    let f = File { mode: FileMode::Link, src: Some("files/rc".into()), ..file("~/.rc") };
    w.apply(&want([f.clone()])).unwrap();
    assert_eq!(fs::read_link(w.at(".rc")).unwrap(), w.spec.join("files/rc"));
    assert!(w.plan(&want([f.clone()])).changes.is_empty());

    // An atomic-rename save swaps the symlink for a file.
    fs::remove_file(w.at(".rc")).unwrap();
    fs::write(w.at(".rc"), "app version\n").unwrap();
    assert_eq!(drift_notes_of(&w.plan(&want([f.clone()]))), ["symlink replaced by a regular file"]);
    w.apply(&want([f.clone()])).unwrap();
    assert_eq!(fs::read_to_string(w.spec.join("files/rc")).unwrap(), "app version\n");
    assert_eq!(fs::read_link(w.at(".rc")).unwrap(), w.spec.join("files/rc"));
    assert!(w.plan(&want([f.clone()])).changes.is_empty());

    w.apply(&[]).unwrap();
    assert!(fs::symlink_metadata(w.at(".rc")).is_err());
    assert!(w.spec.join("files/rc").exists());
}

#[test]
fn a_link_can_point_outside_the_repo_at_a_directory_that_is_not_there_yet() {
    let w = World::new();
    let drive = w.spec.join("../Volumes/Kyan Drive/gradle");
    let f = File { mode: FileMode::Link, target: Some(drive.display().to_string()), ..file("~/.gradle") };
    w.apply(&want([f.clone()])).unwrap();
    assert_eq!(fs::read_link(w.at(".gradle")).unwrap(), drive);
    assert!(w.plan(&want([f.clone()])).changes.is_empty(), "a dangling target is not drift");

    // The app wrote a file where the link was: nothing is copied into the target.
    fs::remove_file(w.at(".gradle")).unwrap();
    fs::write(w.at(".gradle"), "app").unwrap();
    w.apply(&want([f.clone()])).unwrap();
    assert!(!drive.exists());
    assert_eq!(fs::read_link(w.at(".gradle")).unwrap(), drive);

    let home =
        File { mode: FileMode::Link, target: Some("~/.claude/plugins".into()), ..file("~/.claude-pulse/plugins") };
    w.apply(&want([home])).unwrap();
    assert_eq!(fs::read_link(w.at(".claude-pulse/plugins")).unwrap(), w.at(".claude/plugins"));

    for bad in [
        File { mode: FileMode::Link, target: Some("relative/x".into()), ..file("~/a") },
        File { mode: FileMode::Link, target: Some("/x".into()), src: Some("files/x".into()), ..file("~/a") },
    ] {
        assert!(w.apply(&want([bad.clone()])).is_err(), "{bad:?}");
    }
}

#[test]
fn a_link_into_the_repo_can_be_a_directory() {
    let w = World::new();
    fs::create_dir_all(w.spec.join("files/nvim")).unwrap();
    w.write_src("files/nvim/init.lua", "x");
    let f = File { mode: FileMode::Link, src: Some("files/nvim".into()), ..file("~/.config/nvim") };
    w.apply(&want([f.clone()])).unwrap();
    assert_eq!(w.read(".config/nvim/init.lua"), "x");
    assert!(w.plan(&want([f])).changes.is_empty());
}

#[test]
fn link_removal_keeps_a_file_that_replaced_it() {
    let w = World::new();
    w.write_src("files/rc", "x");
    let f = File { mode: FileMode::Link, src: Some("files/rc".into()), ..file("~/.rc") };
    w.apply(&want([f])).unwrap();
    fs::remove_file(w.at(".rc")).unwrap();
    fs::write(w.at(".rc"), "mine").unwrap();
    w.apply(&[]).unwrap();
    assert_eq!(w.read(".rc"), "mine");
}

#[test]
fn seed_is_written_once_and_never_touched() {
    let w = World::new();
    let f = File { mode: FileMode::Seed, ..text("~/.seed", "initial") };
    w.apply(&want([f.clone()])).unwrap();
    assert_eq!(w.read(".seed"), "initial");

    fs::write(w.at(".seed"), "user edit").unwrap();
    assert!(w.plan(&want([f.clone()])).changes.is_empty());
    let changed = File { content: Some("new default".into()), ..f };
    w.apply(&want([changed])).unwrap();
    assert_eq!(w.read(".seed"), "user edit");

    // The edited seed survives removal of the resource; an untouched one does not.
    w.apply(&[]).unwrap();
    assert_eq!(w.read(".seed"), "user edit");

    let again = File { mode: FileMode::Seed, ..text("~/.seed2", "initial") };
    w.apply(&want([again])).unwrap();
    w.apply(&[]).unwrap();
    assert!(!w.at(".seed2").exists());
}

#[test]
fn seed_over_an_existing_file_leaves_it_and_removal_keeps_it() {
    let w = World::new();
    fs::write(w.at(".seed"), "already here").unwrap();
    let f = File { mode: FileMode::Seed, ..text("~/.seed", "initial") };
    w.apply(&want([f])).unwrap();
    assert_eq!(w.read(".seed"), "already here");
    w.apply(&[]).unwrap();
    assert_eq!(w.read(".seed"), "already here");
}

#[test]
fn removal_deletes_only_unedited_managed_files() {
    let w = World::new();
    w.apply(&want([text("~/keep", "a"), text("~/drop", "b")])).unwrap();
    fs::write(w.at("keep"), "edited").unwrap();
    w.apply(&[]).unwrap();
    assert_eq!(w.read("keep"), "edited");
    assert!(!w.at("drop").exists());
    assert!(w.state().entries.is_empty());
}

#[test]
fn a_dangling_link_in_the_way_becomes_a_directory_and_comes_back_on_rollback() {
    let w = World::new();
    std::os::unix::fs::symlink("/nix/store/gone/agents", w.at("agents")).unwrap();
    w.apply(&want([text("~/agents/a.md", "a")])).unwrap();
    assert_eq!(w.read("agents/a.md"), "a");

    let w = World::new();
    std::os::unix::fs::symlink("/nix/store/gone/agents", w.at("agents")).unwrap();
    fs::write(w.at("blocker"), "i am a file").unwrap();
    assert!(w.apply(&want([text("~/agents/a.md", "a"), text("~/blocker/b", "x")])).is_err());
    assert_eq!(fs::read_link(w.at("agents")).unwrap(), Path::new("/nix/store/gone/agents"));
}

#[test]
fn rollback_restores_a_file_when_a_later_op_fails() {
    let w = World::new();
    w.apply(&want([text("~/a", "one")])).unwrap();
    fs::write(w.at("blocker"), "i am a file").unwrap();
    let err = w.apply(&want([text("~/a", "two"), text("~/blocker/b", "x")])).unwrap_err();
    assert!(format!("{err:#}").contains("rolled back"), "{err:#}");
    assert_eq!(w.read("a"), "one");
    assert_eq!(w.state().generation, 1);
}

fn patch_file(path: &str, owns: Value) -> File {
    let owns: BTreeMap<String, Value> = serde_json::from_value(owns).unwrap();
    File { mode: FileMode::Patch, owns, ..file(path) }
}

fn union(items: Value) -> Value {
    json!({"kind": "list_union", "items": items})
}

#[test]
fn patch_edits_only_owned_keys_and_tracks_drift() {
    let w = World::new();
    fs::create_dir_all(w.at(".claude")).unwrap();
    fs::write(w.at(".claude/settings.json"), r#"{"theme": "dark", "model": "sonnet", "permissions": {"deny": ["x"]}}"#)
        .unwrap();
    let owns = json!({"model": "opus", "permissions.allow": union(json!(["rg", "fd"]))});
    let f = patch_file("~/.claude/settings.json", owns);
    w.apply(&want([f.clone()])).unwrap();
    let doc: Value = serde_json::from_str(&w.read(".claude/settings.json")).unwrap();
    assert_eq!(doc, json!({"theme": "dark", "model": "opus", "permissions": {"deny": ["x"], "allow": ["rg", "fd"]}}));
    assert!(w.plan(&want([f.clone()])).changes.is_empty());

    // The app changing an unowned key is not drift; changing an owned one is.
    let edit = |model: &str| {
        let mut doc: Value = serde_json::from_str(&w.read(".claude/settings.json")).unwrap();
        doc["theme"] = "light".into();
        doc["model"] = model.into();
        fs::write(w.at(".claude/settings.json"), doc.to_string()).unwrap();
    };
    edit("opus");
    assert!(w.plan(&want([f.clone()])).changes.is_empty());
    edit("haiku");
    assert_eq!(drift_notes_of(&w.plan(&want([f.clone()]))), ["`model` changed"]);
    w.apply(&want([f.clone()])).unwrap();
    let doc: Value = serde_json::from_str(&w.read(".claude/settings.json")).unwrap();
    assert_eq!((doc["model"].clone(), doc["theme"].clone()), ("opus".into(), "light".into()));
}

#[test]
fn patch_theirs_releases_a_key_until_desired_changes() {
    let w = World::new();
    let mk = |model: &str| File {
        on_conflict: OnConflict::Theirs,
        ..patch_file("~/s.json", json!({"model": model, "other": 1}))
    };
    w.apply(&want([mk("opus")])).unwrap();
    fs::write(w.at("s.json"), r#"{"model": "haiku", "other": 1}"#).unwrap();
    w.apply(&want([mk("opus")])).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&w.read("s.json")).unwrap()["model"], "haiku");
    assert!(w.plan(&want([mk("opus")])).changes.is_empty());

    // Desired changed: managing resumes. The disk still differs from last-applied, and theirs
    // keeps it again, now released for the new value.
    w.apply(&want([mk("sonnet")])).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&w.read("s.json")).unwrap()["model"], "haiku");
    let rec = patch::Record::from_map(&w.state().entries.values().next().unwrap().record);
    assert_eq!(rec.released["model"], "sonnet");
}

#[test]
fn patch_fail_stops_before_anything_is_written() {
    let w = World::new();
    let mk = |model: &str| File { on_conflict: OnConflict::Fail, ..patch_file("~/s.json", json!({"model": model})) };
    w.apply(&want([mk("opus")])).unwrap();
    fs::write(w.at("s.json"), r#"{"model": "haiku"}"#).unwrap();
    let err = format!("{:#}", w.apply(&want([mk("opus"), text("~/other", "x")])).unwrap_err());
    assert!(err.contains("`model`") && err.contains("on_conflict"), "{err}");
    assert!(!w.at("other").exists());
}

#[test]
fn patch_drop_key_and_remove_resource() {
    let w = World::new();
    fs::write(w.at("s.json"), r#"{"keep": 1}"#).unwrap();
    let both = patch_file("~/s.json", json!({"a": 1, "b": 2, "l": union(json!(["x", "y"]))}));
    w.apply(&want([both])).unwrap();

    // `a` was edited by the app, `b` was not: dropping both deletes only `b`. The list loses the
    // items fspec added and keeps the app's.
    fs::write(w.at("s.json"), r#"{"keep": 1, "a": 99, "b": 2, "l": ["x", "y", "mine"]}"#).unwrap();
    let rest = patch_file("~/s.json", json!({"l": union(json!(["x"]))}));
    w.apply(&want([rest.clone()])).unwrap();
    let doc: Value = serde_json::from_str(&w.read("s.json")).unwrap();
    assert_eq!(doc, json!({"keep": 1, "a": 99, "l": ["x", "mine"]}));

    w.apply(&[]).unwrap();
    let doc: Value = serde_json::from_str(&w.read("s.json")).unwrap();
    assert_eq!(doc, json!({"keep": 1, "a": 99, "l": ["mine"]}));
}

#[test]
fn patch_creates_a_missing_file_in_toml_and_yaml() {
    let w = World::new();
    w.apply(&want([
        patch_file("~/c.toml", json!({"editor.theme": "nord", "editor.tabs": 4})),
        patch_file("~/c.yaml", json!({"a.b": [1, 2]})),
    ]))
    .unwrap();
    assert_eq!(w.read("c.toml"), "[editor]\ntabs = 4\ntheme = \"nord\"\n");
    assert_eq!(w.read("c.yaml"), "a:\n  b:\n  - 1\n  - 2\n");
}

fn merge_one(disk: Value, owns: Value, last: Value, policy: OnConflict) -> (Value, patch::Record, Vec<String>) {
    let owns: BTreeMap<String, Value> = serde_json::from_value(owns).unwrap();
    let last: BTreeMap<String, Value> = serde_json::from_value(last).unwrap();
    let record = patch::Record { keys: last, released: Default::default() };
    let m = patch::merge(&disk, &owns, &owns, &record, policy).unwrap();
    (m.doc, m.record, m.conflicts)
}

#[test]
fn merge_table_rows() {
    use OnConflict::*;
    // equals last-applied: untouched, write the new desired value.
    let (doc, rec, _) = merge_one(json!({"k": "old"}), json!({"k": "new"}), json!({"k": "old"}), Fail);
    assert_eq!((doc, rec.keys["k"].clone()), (json!({"k": "new"}), json!("new")));

    // differs: ours wins, theirs keeps and releases, fail reports.
    let disk = json!({"k": "app"});
    let (doc, rec, _) = merge_one(disk.clone(), json!({"k": "new"}), json!({"k": "old"}), Ours);
    assert_eq!((doc, rec.keys["k"].clone()), (json!({"k": "new"}), json!("new")));
    let (doc, rec, _) = merge_one(disk.clone(), json!({"k": "new"}), json!({"k": "old"}), Theirs);
    assert_eq!((doc, rec.keys["k"].clone(), rec.released["k"].clone()), (disk.clone(), json!("old"), json!("new")));
    let (doc, _, conflicts) = merge_one(disk.clone(), json!({"k": "new"}), json!({"k": "old"}), Fail);
    assert_eq!((doc, conflicts), (disk, vec!["k".to_string()]));

    // missing: deleted counts as a change.
    let (doc, _, conflicts) = merge_one(json!({}), json!({"k": "new"}), json!({"k": "old"}), Fail);
    assert_eq!((doc, conflicts), (json!({}), vec!["k".to_string()]));
    let (doc, ..) = merge_one(json!({}), json!({"k": "new"}), json!({"k": "old"}), Ours);
    assert_eq!(doc, json!({"k": "new"}));

    // never applied and absent: plain write. Already equal to desired: no conflict, record adopts it.
    let (doc, ..) = merge_one(json!({}), json!({"k": 1}), json!({}), Fail);
    assert_eq!(doc, json!({"k": 1}));
    let (_, rec, conflicts) = merge_one(json!({"k": 1}), json!({"k": 1}), json!({"k": 0}), Fail);
    assert_eq!((rec.keys["k"].clone(), conflicts.len()), (json!(1), 0));
}

#[test]
fn list_strategies() {
    use OnConflict::*;
    let strategy = |kind: &str, items: Value| json!({"kind": kind, "items": items});

    // union keeps the app's items and adds ours once.
    let owns = json!({"l": strategy("list_union", json!(["a", "b"]))});
    let (doc, ..) = merge_one(json!({"l": ["z", "a"]}), owns.clone(), json!({}), Fail);
    assert_eq!(doc, json!({"l": ["z", "a", "b"]}));
    // Removing one of our items from the disk is a conflict.
    let (doc, ..) = merge_one(json!({"l": ["z", "a"]}), owns.clone(), json!({"l": ["a", "b"]}), Ours);
    assert_eq!(doc, json!({"l": ["z", "a", "b"]}));
    let (doc, _, conflicts) = merge_one(json!({"l": ["z", "a"]}), owns, json!({"l": ["a", "b"]}), Fail);
    assert_eq!((doc, conflicts), (json!({"l": ["z", "a"]}), vec!["l".to_string()]));

    // replace is exact.
    let owns = json!({"l": strategy("list_replace", json!(["a"]))});
    let (doc, ..) = merge_one(json!({"l": ["a", "z"]}), owns, json!({"l": ["a", "z"]}), Fail);
    assert_eq!(doc, json!({"l": ["a"]}));

    // keyed merges objects by field, replaces ours in place, keeps the app's.
    let keyed = |items: Value| json!({"kind": "list_keyed", "field": "m", "items": items});
    let owns = json!({"h": keyed(json!([{"m": "bash", "c": 2}, {"m": "edit", "c": 1}]))});
    let disk = json!({"h": [{"m": "app", "c": 9}, {"m": "bash", "c": 1}]});
    let last = json!({"h": [{"m": "bash", "c": 1}]});
    let (doc, rec, _) = merge_one(disk, owns, last, Fail);
    assert_eq!(doc, json!({"h": [{"m": "app", "c": 9}, {"m": "bash", "c": 2}, {"m": "edit", "c": 1}]}));
    assert_eq!(rec.keys["h"], json!([{"m": "bash", "c": 2}, {"m": "edit", "c": 1}]));

    let bad = json!({"h": keyed(json!([{"c": 1}]))});
    let bad: BTreeMap<String, Value> = serde_json::from_value(bad).unwrap();
    assert!(patch::Want::parse("h", &bad["h"]).is_err());
}

fn age_fixture(w: &World, plain: &str) -> String {
    let id = age::x25519::Identity::generate();
    let mut ciphertext = vec![];
    let enc = age::Encryptor::with_recipients(std::iter::once(&id.to_public() as &dyn age::Recipient)).unwrap();
    let mut out = enc.wrap_output(&mut ciphertext).unwrap();
    std::io::Write::write_all(&mut out, plain.as_bytes()).unwrap();
    out.finish().unwrap();
    fs::write(w.spec.join("token.age"), ciphertext).unwrap();
    let key_dir = w.paths.home.join(".config/fspec");
    fs::create_dir_all(&key_dir).unwrap();
    use age::secrecy::ExposeSecret;
    fs::write(key_dir.join("age.key"), format!("{}\n", id.to_string().expose_secret())).unwrap();
    "token.age".into()
}

#[test]
fn secret_decrypts_with_perms_owner_and_drift() {
    let w = World::new();
    let src = age_fixture(&w, "s3cret\n");
    let s = Resource::Secret(Secret {
        path: "~/.config/token".into(),
        src,
        perms: "0400".into(),
        owner: Some("root:wheel".into()),
        identities: vec![],
    });
    let want = vec![Desired { resource: s, owner: None }];
    w.apply(&want).unwrap();
    assert_eq!(w.read(".config/token"), "s3cret\n");
    assert_eq!(fs::metadata(w.at(".config/token")).unwrap().permissions().mode() & 0o7777, 0o400);
    assert!(w.runner.lines().iter().any(|l| l.starts_with("chown root:wheel ") && l.ends_with(".config/token")));
    assert!(w.plan(&want).changes.is_empty());

    fs::set_permissions(w.at(".config/token"), fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(w.at(".config/token"), "leaked").unwrap();
    assert_eq!(drift_notes_of(&w.plan(&want)), ["content changed"]);
    w.apply(&want).unwrap();
    assert_eq!(w.read(".config/token"), "s3cret\n");

    w.apply(&[]).unwrap();
    assert!(!w.at(".config/token").exists());
}

const TEST_SSH_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACDoh10A29aO/vlbh1q5CkfgtibH0F0YO1w8VMWX3bNXdwAAAJDlQWwQ5UFs
EAAAAAtzc2gtZWQyNTUxOQAAACDoh10A29aO/vlbh1q5CkfgtibH0F0YO1w8VMWX3bNXdw
AAAEAYymhLTGAcxGs16xzJ7kCRcERnBnaXJs9H4fWdI+69oeiHXQDb1o7++VuHWrkKR+C2
JsfQXRg7XDxUxZfds1d3AAAACmZzcGVjLXRlc3QBAgM=
-----END OPENSSH PRIVATE KEY-----
";
const TEST_SSH_PUB: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOiHXQDb1o7++VuHWrkKR+C2JsfQXRg7XDxUxZfds1d3 fspec-test";

#[test]
fn secret_decrypts_with_the_users_ssh_key_and_reinstalls_after_a_wipe() {
    let w = World::new();
    fs::create_dir_all(w.at(".ssh")).unwrap();
    fs::write(w.at(".ssh/id_ed25519"), TEST_SSH_KEY).unwrap();
    let ciphertext = crate::secret::encrypt(b"tok\n", &[TEST_SSH_PUB.to_string()]).unwrap();
    fs::write(w.spec.join("tok.age"), ciphertext).unwrap();
    let s = Resource::Secret(Secret {
        path: "~/run/tok".into(),
        src: "tok.age".into(),
        perms: "0400".into(),
        owner: None,
        identities: vec![],
    });
    w.apply(&[Desired { resource: s, owner: None }]).unwrap();
    assert_eq!(w.read("run/tok"), "tok\n");

    fs::remove_file(w.at("run/tok")).unwrap();
    fs::remove_file(w.spec.join("tok.age")).unwrap();
    let written = crate::reinstall_secrets(&w.paths, Scope::User, &w.state(), &w.runner).unwrap();
    assert_eq!(written, [w.at("run/tok")]);
    assert_eq!(w.read("run/tok"), "tok\n");
    assert_eq!(fs::metadata(w.at("run/tok")).unwrap().permissions().mode() & 0o7777, 0o400);
}

#[test]
fn secret_identities_replace_the_defaults() {
    let w = World::new();
    let src = age_fixture(&w, "x");
    fs::rename(w.at(".config/fspec/age.key"), w.at("keys.txt")).unwrap();
    let s = Resource::Secret(Secret {
        path: "~/t".into(),
        src,
        perms: "0400".into(),
        owner: None,
        identities: vec!["~/.ssh/missing".into(), "~/keys.txt".into()],
    });
    w.apply(&[Desired { resource: s, owner: None }]).unwrap();
    assert_eq!(w.read("t"), "x");
}

#[test]
fn secret_without_the_identity_fails_before_changing_anything() {
    let w = World::new();
    age_fixture(&w, "x");
    fs::remove_file(w.at(".config/fspec/age.key")).unwrap();
    let s = Resource::Secret(Secret {
        path: "~/t".into(),
        src: "token.age".into(),
        perms: "0400".into(),
        owner: None,
        identities: vec![],
    });
    let err = format!("{:#}", w.apply(&[Desired { resource: s, owner: None },]).unwrap_err());
    assert!(err.contains("age.key"), "{err}");
    assert!(!w.at("t").exists());
}

fn watched(f: File, record: Map<String, Value>) -> Watched {
    Watched { key: "file:x".into(), path: "/x".into(), resource: Resource::File(f), record }
}

#[test]
fn watch_reports_owned_changes_once_and_ignores_unowned_ones() {
    let owns = json!({"model": "opus"});
    let f = patch_file("~/s.json", owns);
    let record = patch::Record { keys: [("model".to_string(), json!("opus"))].into(), released: Default::default() };
    let w = watched(f, record.to_map());
    let live = |text: &str| Live::File(text.as_bytes().to_vec());

    assert_eq!(decide(&w, &live(r#"{"model": "opus"}"#), &[]), None);
    assert_eq!(decide(&w, &live(r#"{"model": "opus", "theme": "light"}"#), &[]), None);
    let drifted = decide(&w, &live(r#"{"model": "haiku"}"#), &[]).unwrap();
    assert_eq!(drifted, ["`model` changed"]);
    assert_eq!(decide(&w, &live(r#"{"model": "haiku", "x": 1}"#), &drifted), None);
    assert_eq!(decide(&w, &live(r#"{"model": "opus"}"#), &drifted), Some(vec![]));
}

#[test]
fn watch_decisions_for_managed_and_link() {
    let m = watched(text("~/a", "x"), hash_record(b"x"));
    assert_eq!(decide(&m, &Live::File(b"x".to_vec()), &[]), None);
    assert_eq!(decide(&m, &Live::File(b"y".to_vec()), &[]).unwrap(), ["content changed"]);
    assert_eq!(decide(&m, &Live::Missing, &[]).unwrap(), ["file is missing"]);

    let mut rec = Map::new();
    rec.insert("target".into(), "/repo/rc".into());
    let l = watched(File { mode: FileMode::Link, src: Some("rc".into()), ..file("~/.rc") }, rec);
    assert_eq!(decide(&l, &Live::Link("/repo/rc".into()), &[]), None);
    assert_eq!(decide(&l, &Live::File(b"z".to_vec()), &[]).unwrap(), ["symlink replaced by a regular file"]);
}

#[test]
fn watch_sees_a_real_edit() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    let w = World::new();
    w.apply(&want([text("~/watched.txt", "one")])).unwrap();
    let lines = Arc::new(Mutex::new(Vec::<String>::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let handle = {
        let (paths, lines, stop) = (w.paths.clone(), lines.clone(), stop.clone());
        std::thread::spawn(move || {
            watch::run(&paths, &mut |l| lines.lock().unwrap().push(l), &stop).unwrap();
        })
    };
    std::thread::sleep(Duration::from_millis(500));
    fs::write(w.at("watched.txt"), "two").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && !lines.lock().unwrap().iter().any(|l| l.starts_with("drift ")) {
        std::thread::sleep(Duration::from_millis(100));
    }
    stop.store(true, Ordering::Relaxed);
    handle.join().unwrap();
    let lines = lines.lock().unwrap().clone();
    assert!(lines.iter().any(|l| l == "drift file:~/watched.txt: content changed"), "{lines:?}");
}

#[test]
fn adopt_copies_src_files_refuses_generated_and_lists_patch_keys() {
    let w = World::new();
    w.write_src("files/rc", "v1\n");
    let managed = File { src: Some("files/rc".into()), ..file("~/.rc") };
    let generated = text("~/gen.txt", "a\nb\n");
    let patch = patch_file("~/p.json", json!({"model": "opus"}));
    w.apply(&want([managed.clone(), generated.clone(), patch.clone()])).unwrap();
    let state = w.state();
    let record = |f: &File| state.entries[&Resource::File(f.clone()).key()].record.clone();

    assert_eq!(adopt(&w.cx(), &managed, &record(&managed)).unwrap(), Outcome::InSync);
    fs::write(w.at(".rc"), "v2 from the app\n").unwrap();
    let out = adopt(&w.cx(), &managed, &record(&managed)).unwrap();
    let Outcome::Copied { to, record: adopted } = out else { panic!() };
    assert_eq!(to, w.spec.join("files/rc"));
    assert_eq!(adopted["sha256"], sha256(b"v2 from the app\n"));
    assert_eq!(fs::read_to_string(w.spec.join("files/rc")).unwrap(), "v2 from the app\n");

    fs::write(w.at("gen.txt"), "a\nc\n").unwrap();
    let Outcome::Generated { diff } = adopt(&w.cx(), &generated, &record(&generated)).unwrap() else { panic!() };
    assert!(diff.contains("-b") && diff.contains("+c"), "{diff}");
    assert_eq!(w.read("gen.txt"), "a\nc\n");

    fs::write(w.at("p.json"), r#"{"model": "haiku"}"#).unwrap();
    let out = adopt(&w.cx(), &patch, &record(&patch)).unwrap();
    assert_eq!(out, Outcome::Patch { notes: vec!["`model` changed".into()] });
}

#[test]
fn protected_paths_fail_the_plan() {
    let w = World::new();
    let protect = vec!["~/.ssh".to_string()];
    let cx = Cx { protect: &protect, ..w.cx() };
    for path in ["~/.ssh/config", "~/.ssh/../.ssh/config"] {
        let plan = plan_scope(&cx, &[&FileBackend], &want([text(path, "x")]), &State::default()).unwrap();
        assert!(plan.unsupported.iter().any(|u| u.contains("protected path")), "{path}: {plan:?}");
    }
    let secret = Desired {
        resource: Resource::Secret(Secret {
            path: "~/.ssh/key".into(),
            src: "secrets/k.age".into(),
            perms: "0600".into(),
            owner: None,
            identities: vec![],
        }),
        owner: None,
    };
    assert!(!plan_scope(&cx, &[&FileBackend], &[secret], &State::default()).unwrap().unsupported.is_empty());
    let ok = plan_scope(&cx, &[&FileBackend], &want([text("~/.config/ok", "x")]), &State::default()).unwrap();
    assert!(ok.unsupported.is_empty());
}

#[test]
fn a_unit_file_written_as_a_file_reloads_systemd() {
    let mut w = World::new();
    w.facts.init = fspec_core::facts::Init::Systemd;
    w.apply(&want([text("~/.config/notes.txt", "a")])).unwrap();
    assert!(w.runner.lines().is_empty());
    w.apply(&want([text("~/.config/notes.txt", "a"), text("~/.config/systemd/user/app.service", "[Unit]\n")])).unwrap();
    assert_eq!(w.runner.lines(), ["systemctl --user daemon-reload"]);
}

/// Stands in for the package that creates a link's target: it sees whether the link was already
/// there when its phase committed, then creates the target.
struct Provides {
    target: PathBuf,
    link: PathBuf,
    saw_link: std::cell::Cell<Option<bool>>,
}

impl fspec_core::Backend for Provides {
    fn name(&self) -> &'static str {
        "provides"
    }

    fn handles(&self, _cx: &Cx, r: &Resource) -> bool {
        matches!(r, Resource::Pkg(_))
    }

    fn phase(&self) -> Phase {
        Phase::Packages
    }

    fn commit(&self, _cx: &Cx, _ops: &[Op], _staging: &Path, _journal: &mut Journal) -> Result<Records> {
        self.saw_link.set(Some(fs::symlink_metadata(&self.link).is_ok()));
        fs::create_dir_all(self.target.parent().unwrap())?;
        fs::write(&self.target, "nameserver 127.0.0.53\n")?;
        Ok(Records::new())
    }
}

#[test]
fn a_link_to_a_target_a_later_phase_creates_waits_for_that_phase() {
    let w = World::new();
    let cx = Cx { scope: Scope::System, ..w.cx() };
    let target = w.paths.resolve("/run/resolve/stub-resolv.conf").unwrap();
    let live = w.paths.resolve("/etc/resolv.conf").unwrap();
    let link =
        File { mode: FileMode::Link, target: Some("/run/resolve/stub-resolv.conf".into()), ..file("/etc/resolv.conf") };
    let pkg: Resource =
        serde_json::from_value(json!({"kind": "pkg", "name": "resolved", "source": {"kind": "aqua", "name": "x"}}))
            .unwrap();
    let provides = Provides { target, link: live.clone(), saw_link: Default::default() };
    let mut desired = want([link]);
    desired.push(Desired { resource: pkg, owner: None });
    let backends: [&dyn fspec_core::Backend; 2] = [&FileBackend, &provides];
    apply_scope(&cx, &backends, &desired).unwrap();
    assert_eq!(provides.saw_link.get(), Some(false));
    assert_eq!(fs::read_link(&live).unwrap(), PathBuf::from("/run/resolve/stub-resolv.conf"));
    let cx = Cx { scope: Scope::System, ..w.cx() };
    let state = State::load(&state_dir(&w.paths, Scope::System).join("state.json")).unwrap();
    assert!(plan_scope(&cx, &backends, &desired, &state).unwrap().changes.is_empty(), "the deferred link is recorded");
}
