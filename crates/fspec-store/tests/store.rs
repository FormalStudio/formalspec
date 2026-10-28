use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use fspec_core::apply::{apply_scope, desired_from_generation, plan_scope, state_dir};
use fspec_core::fetch::MapFetch;
use fspec_core::lock::{Artifact, LockedPkg};
use fspec_core::runner::{Cmd, Output, RecordingRunner, Runner};
use fspec_core::{Backend, Cx, Desired, Facts, Lock, Paths, Plan, Scope, State};
use fspec_store::{StoreBackend, lock_from_state, sri_sha256};
use serde_json::json;

const PLATFORM: &str = "aarch64-darwin";

fn tgz(files: &[(&str, &str, u32)]) -> Vec<u8> {
    let mut tar = tar::Builder::new(Vec::new());
    for (path, content, mode) in files {
        let mut h = tar::Header::new_gnu();
        h.set_size(content.len() as u64);
        h.set_mode(*mode);
        tar.append_data(&mut h, path, content.as_bytes()).unwrap();
    }
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(&tar.into_inner().unwrap()).unwrap();
    gz.finish().unwrap()
}

fn hello(version: &str) -> Vec<u8> {
    tgz(&[(&format!("hello-{version}/hello"), &format!("#!/bin/sh\necho {version}\n"), 0o755)])
}

fn locked(name: &str, source: &str, version: &str, url: &str, body: &[u8]) -> LockedPkg {
    let artifact = Artifact { url: Some(url.into()), hash: Some(sri_sha256(body)), ..Default::default() };
    LockedPkg {
        name: name.into(),
        source: source.into(),
        version: Some(version.into()),
        rev: None,
        patches: None,
        parts: Default::default(),
        artifacts: BTreeMap::from([(PLATFORM.to_string(), artifact)]),
    }
}

fn pkg(name: &str, source: serde_json::Value, mode: &str) -> Desired {
    let resource = serde_json::from_value(json!({"kind": "pkg", "name": name, "source": source, "mode": mode}));
    Desired { resource: resource.unwrap(), owner: None }
}

fn aqua(name: &str) -> Desired {
    pkg(name, json!({"kind": "aqua", "name": name}), "pinned")
}

struct World {
    _dir: tempfile::TempDir,
    paths: Paths,
    build_cache: PathBuf,
    facts: Facts,
    lock: Lock,
    runner: Box<dyn Runner>,
    fetch: MapFetch,
}

impl World {
    fn new() -> World {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().into(), dir.path().join("home/u"));
        World {
            build_cache: dir.path().join("build-cache"),
            _dir: dir,
            paths,
            facts: Facts::declared("m", PLATFORM, None).unwrap(),
            lock: Lock::default(),
            runner: Box::new(RecordingRunner::default()),
            fetch: MapFetch::default(),
        }
    }

    fn cx(&self) -> Cx<'_> {
        let generation = self.state().generation + 1;
        Cx {
            paths: &self.paths,
            facts: &self.facts,
            spec_dir: &self.paths.root,
            lock: &self.lock,
            runner: self.runner.as_ref(),
            fetch: &self.fetch,
            generation,
            scope: Scope::System,
            protect: &[],
            builders: &[],
            build_cache: Some(&self.build_cache),
        }
    }

    fn state(&self) -> State {
        State::load(&self.state_dir().join("state.json")).unwrap()
    }

    fn state_dir(&self) -> PathBuf {
        state_dir(&self.paths, Scope::System)
    }

    fn apply(&self, want: &[Desired]) -> anyhow::Result<Plan> {
        apply_scope(&self.cx(), &[&StoreBackend as &dyn Backend], want)
    }

    fn plan(&self, want: &[Desired]) -> Plan {
        plan_scope(&self.cx(), &[&StoreBackend as &dyn Backend], want, &self.state()).unwrap()
    }

    fn root(&self) -> PathBuf {
        self.paths.fspec_root()
    }

    fn current(&self) -> PathBuf {
        fs::read_link(self.root().join("current")).unwrap()
    }

    fn run(&self, bin: &str) -> String {
        let out = std::process::Command::new(self.root().join("current/bin").join(bin)).output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn store_entries(&self) -> usize {
        fs::read_dir(self.root().join("store")).map(|d| d.count()).unwrap_or(0)
    }

    fn add_hello(&mut self, version: &str) {
        let url = format!("https://example/hello-{version}.tar.gz");
        let body = hello(version);
        self.lock.upsert(locked("hello", "aqua:hello", version, &url, &body));
        self.fetch = std::mem::take(&mut self.fetch).with(&url, body);
    }
}

#[test]
fn applies_switches_removes_and_rolls_back() {
    let mut w = World::new();
    let want = [aqua("hello")];

    w.add_hello("1.0.0");
    w.apply(&want).unwrap();
    assert_eq!(w.current(), Path::new("generations/1"));
    assert_eq!(w.run("hello"), "1.0.0");
    assert!(w.plan(&want).changes.is_empty());
    let record = &w.state().entries[&want[0].resource.key()].record;
    assert_eq!(record["version"], "1.0.0");
    assert_eq!(record["bins"]["hello"], "hello-1.0.0/hello");
    let store_v1 = record["store"].as_str().unwrap().to_string();

    // A newer lock shows up as drift and builds a new generation beside the old one.
    w.add_hello("2.0.0");
    let plan = w.plan(&want);
    assert_eq!(plan.changes.len(), 1, "{plan}");
    w.apply(&want).unwrap();
    assert_eq!(w.current(), Path::new("generations/2"));
    assert_eq!(w.run("hello"), "2.0.0");
    assert!(w.plan(&want).changes.is_empty());
    assert!(w.root().join("generations/1/bin/hello").exists());
    assert_eq!(w.store_entries(), 4);

    // Rollback needs no network: the old store path is reused and only `current` moves.
    let dir = w.state_dir();
    let past = State::load(&dir.join("generations/1.json")).unwrap();
    let desired = desired_from_generation(&dir, 1).unwrap();
    w.lock = lock_from_state(&past);
    w.fetch = MapFetch::default();
    w.apply(&desired).unwrap();
    assert_eq!(w.current(), Path::new("generations/3"));
    assert_eq!(w.run("hello"), "1.0.0");
    assert_eq!(w.state().entries[&want[0].resource.key()].record["store"], store_v1);
    assert_eq!(w.store_entries(), 4);
    assert!(w.plan(&want).changes.is_empty());

    w.apply(&[]).unwrap();
    assert_eq!(w.current(), Path::new("generations/4"));
    assert!(!w.root().join("current/bin/hello").exists());
    assert!(w.state().entries.is_empty());
    assert_eq!(w.store_entries(), 4);
}

#[test]
fn hash_mismatch_aborts_before_anything_changes() {
    let mut w = World::new();
    w.add_hello("1.0.0");
    w.fetch = MapFetch::default().with("https://example/hello-1.0.0.tar.gz", hello("9.9.9"));
    let err = w.apply(&[aqua("hello")]).unwrap_err();
    assert!(format!("{err:#}").contains("hash mismatch"), "{err:#}");
    assert!(fs::symlink_metadata(w.root().join("current")).is_err());
    assert_eq!(w.store_entries(), 0);
    assert!(!w.root().join("generations").exists());
    assert_eq!(w.state().generation, 0);
}

#[test]
fn names_the_fix_when_the_lock_does_not_cover_a_pkg() {
    let mut w = World::new();
    let err = w.apply(&[aqua("hello")]).unwrap_err();
    assert!(format!("{err:#}").contains("pkg hello is not in fspec.lock, run fspec update"), "{err:#}");

    w.add_hello("1.0.0");
    let github = pkg("hello", json!({"kind": "github", "repo": "acme/hello"}), "pinned");
    let err = w.apply(&[github]).unwrap_err();
    assert!(format!("{err:#}").contains("pkg hello is not in fspec.lock, run fspec update"), "{err:#}");
}

#[test]
fn self_managed_installs_once_and_ignores_later_locks() {
    let mut w = World::new();
    let want = [pkg("hello", json!({"kind": "aqua", "name": "hello"}), "self_managed")];
    w.add_hello("1.0.0");
    w.apply(&want).unwrap();

    w.add_hello("2.0.0");
    assert!(w.plan(&want).changes.is_empty());
    w.apply(&want).unwrap();
    assert_eq!(w.state().generation, 1);
    assert_eq!(w.run("hello"), "1.0.0");

    // A config change on the package keeps the installed version too.
    let mut with_data = serde_json::to_value(&want[0].resource).unwrap();
    with_data["data"] = json!(["~/.hello"]);
    let with_data = Desired { resource: serde_json::from_value(with_data).unwrap(), owner: None };
    w.apply(std::slice::from_ref(&with_data)).unwrap();
    assert_eq!(w.state().generation, 2);
    assert_eq!(w.run("hello"), "1.0.0");
}

#[test]
fn a_package_missing_what_the_spec_names_fails_before_any_commit() {
    let mut w = World::new();
    w.add_hello("1.0.0");
    let empty = tgz(&[("lib/readme.txt", "no executables here", 0o644)]);
    w.lock.upsert(locked("zlib", "aqua:zlib", "1.0.0", "https://example/zlib.tar.gz", &empty));
    w.fetch = std::mem::take(&mut w.fetch).with("https://example/zlib.tar.gz", empty);

    let err = w.apply(&[aqua("hello"), aqua("zlib")]).unwrap_err();
    assert!(format!("{err:#}").contains("no executables"), "{err:#}");
    assert!(format!("{err:#}").contains("checking zlib"), "{err:#}");
    assert!(!format!("{err:#}").contains("rolled back"), "{err:#}");
    assert!(fs::symlink_metadata(w.root().join("current")).is_err());
    assert!(!w.root().join("generations/1").exists());
    assert_eq!(w.store_entries(), 0);
    assert_eq!(w.state().generation, 0);
}

#[test]
fn a_failed_second_apply_restores_current() {
    let mut w = World::new();
    w.add_hello("1.0.0");
    w.apply(&[aqua("hello")]).unwrap();

    let empty = tgz(&[("lib/readme.txt", "no executables here", 0o644)]);
    w.lock.upsert(locked("zlib", "aqua:zlib", "1.0.0", "https://example/zlib.tar.gz", &empty));
    w.fetch = std::mem::take(&mut w.fetch).with("https://example/zlib.tar.gz", empty);
    w.apply(&[aqua("hello"), aqua("zlib")]).unwrap_err();
    assert_eq!(w.current(), Path::new("generations/1"));
    assert_eq!(w.run("hello"), "1.0.0");
    assert!(!w.root().join("generations/2").exists());
}

#[test]
fn artifact_bins_pick_the_links() {
    let mut w = World::new();
    let body = tgz(&[
        ("tool-1/bin/tool", "#!/bin/sh\necho tool\n", 0o755),
        ("tool-1/bin/helper", "#!/bin/sh\necho helper\n", 0o755),
    ]);
    let mut l = locked("tool", "aqua:tool", "1.0.0", "https://example/tool.tar.gz", &body);
    l.artifacts.get_mut(PLATFORM).unwrap().bins = vec!["tool-1/bin/tool".into()];
    w.lock.upsert(l);
    w.fetch = MapFetch::default().with("https://example/tool.tar.gz", body);
    w.apply(&[aqua("tool")]).unwrap();
    assert_eq!(w.run("tool"), "tool");
    assert!(!w.root().join("current/bin/helper").exists());
}

fn with_fields(d: Desired, fields: serde_json::Value) -> Desired {
    let mut v = serde_json::to_value(&d.resource).unwrap();
    v.as_object_mut().unwrap().extend(fields.as_object().unwrap().clone());
    Desired { resource: serde_json::from_value(v).unwrap(), owner: None }
}

fn lock_tool(w: &mut World, body: Vec<u8>) {
    w.lock.upsert(locked("tool", "aqua:tool", "1.0.0", "https://example/tool.tar.gz", &body));
    w.fetch = MapFetch::default().with("https://example/tool.tar.gz", body);
}

#[test]
fn a_prefix_tree_links_its_bin_and_keeps_lib_private() {
    let mut w = World::new();
    lock_tool(
        &mut w,
        tgz(&[
            ("formalshell/bin/formalshell", "#!/bin/sh\necho shell\n", 0o755),
            ("formalshell/lib/formalshell/bin/matugen", "#!/bin/sh\necho private\n", 0o755),
            ("formalshell/lib/formalshell/formalshell-rs", "#!/bin/sh\n", 0o755),
            ("formalshell/share/formalshell/VERSION", "0.2.0", 0o644),
        ]),
    );
    w.apply(&[aqua("tool")]).unwrap();
    assert_eq!(w.run("formalshell"), "shell");
    let linked: Vec<_> = fs::read_dir(w.root().join("current/bin")).unwrap().flatten().map(|e| e.file_name()).collect();
    assert_eq!(linked, ["formalshell"]);
}

#[test]
fn a_distro_package_links_its_usr_bin() {
    let mut w = World::new();
    lock_tool(
        &mut w,
        tgz(&[
            (".PKGINFO", "pkgname = asusctl\n", 0o644),
            ("etc/asusd/asusd.ron", "()", 0o644),
            ("usr/bin/asusd", "#!/bin/sh\necho asusd\n", 0o755),
            ("usr/bin/asusctl", "#!/bin/sh\necho asusctl\n", 0o755),
            ("usr/lib/udev/rules.d/99-asusd.rules", "", 0o644),
        ]),
    );
    w.apply(&[with_fields(aqua("tool"), json!({"bins": ["asusd", "asusctl"]}))]).unwrap();
    assert_eq!(w.run("asusd"), "asusd");
    assert_eq!(w.run("asusctl"), "asusctl");
}

#[test]
fn pkg_bins_pick_among_the_executables() {
    let mut w = World::new();
    lock_tool(
        &mut w,
        tgz(&[
            ("files/Discord", "#!/bin/sh\necho discord\n", 0o755),
            ("files/chrome_crashpad_handler", "#!/bin/sh\n", 0o755),
            ("files/libffmpeg.so", "elf", 0o755),
        ]),
    );
    w.apply(&[with_fields(aqua("tool"), json!({"bins": ["Discord"]}))]).unwrap();
    assert_eq!(w.run("Discord"), "discord");
    assert!(!w.root().join("current/bin/libffmpeg.so").exists());

    let err = w.apply(&[with_fields(aqua("tool"), json!({"bins": ["discord"]}))]).unwrap_err();
    assert!(format!("{err:#}").contains("bins: `discord` not among its executables (Discord, "), "{err:#}");
}

#[test]
fn a_url_elf_is_the_command_and_any_other_file_is_data() {
    let mut w = World::new();
    let appimage = b"\x7fELF\x02\x01\x01 AppImage".to_vec();
    let font = b"OTTO font".to_vec();
    w.lock.upsert(locked(
        "beeper",
        "url:https://example/Beeper-4.3.73-x86_64.AppImage",
        "1",
        "https://example/Beeper-4.3.73-x86_64.AppImage",
        &appimage,
    ));
    w.lock.upsert(locked("font", "url:https://example/Geist.otf", "1", "https://example/Geist.otf", &font));
    w.fetch = MapFetch::default()
        .with("https://example/Beeper-4.3.73-x86_64.AppImage", appimage)
        .with("https://example/Geist.otf", font);
    let url = |name: &str, u: &str| pkg(name, json!({"kind": "url", "url": u}), "pinned");
    w.apply(&[url("beeper", "https://example/Beeper-4.3.73-x86_64.AppImage")]).unwrap();
    let beeper = w.root().join("current/bin/beeper");
    assert!(fs::metadata(&beeper).unwrap().permissions().mode() & 0o111 != 0);

    let err = w.apply(&[url("font", "https://example/Geist.otf")]).unwrap_err();
    assert!(format!("{err:#}").contains("no executables"), "{err:#}");
}

#[test]
fn a_url_build_installs_from_the_download_and_reinstalls_when_it_changes() {
    let mut w = World::new();
    let appimage = b"\x7fELF\x02\x01\x01 AppImage".to_vec();
    let url = "https://example/Beeper-4.3.73-x86_64.AppImage";
    w.lock.upsert(locked("beeper", &format!("url:{url}"), "1", url, &appimage));
    w.fetch = MapFetch::default().with(url, appimage);
    w.runner = Box::new(BuildHere);
    let build = |echo: &str| {
        let script = format!(
            r#"test -f "$FSPEC_SRC/beeper" && mkdir -p "$FSPEC_OUT/bin" && printf '#!/bin/sh\necho {echo}\n' > "$FSPEC_OUT/bin/beeper" && chmod +x "$FSPEC_OUT/bin/beeper""#
        );
        with_fields(pkg("beeper", json!({"kind": "url", "url": url}), "pinned"), json!({"build": ["sh", "-c", script]}))
    };
    w.apply(&[build("patched")]).unwrap();
    assert_eq!(w.run("beeper"), "patched");
    w.apply(&[build("patched again")]).unwrap();
    assert_eq!(w.run("beeper"), "patched again");

    let failing = with_fields(
        pkg("beeper", json!({"kind": "url", "url": url}), "pinned"),
        json!({"build": ["sh", "-c", "exit 3"]}),
    );
    let err = w.apply(&[failing]).unwrap_err();
    assert!(format!("{err:#}").contains("building beeper"), "{err:#}");
}

#[test]
fn a_data_only_pkg_links_its_share_trees() {
    let mut w = World::new();
    let fedora = serde_json::from_value(json!({"kind": "distro", "distro": "fedora", "init": "systemd"})).unwrap();
    w.facts = Facts::declared("m", "x86_64-linux", Some(&fedora)).unwrap();
    let body = tgz(&[
        ("Bibata-Modern-Classic/index.theme", "[Icon Theme]", 0o644),
        ("Bibata-Modern-Classic/cursors/left_ptr", "cursor", 0o644),
    ]);
    let mut l = locked("bibata", "url:https://example/Bibata.tar.gz", "2.0.7", "https://example/Bibata.tar.gz", &body);
    l.artifacts = BTreeMap::from([("x86_64-linux".to_string(), l.artifacts.remove(PLATFORM).unwrap())]);
    w.lock.upsert(l);
    w.fetch = MapFetch::default().with("https://example/Bibata.tar.gz", body);
    let bibata = |share: serde_json::Value| {
        with_fields(
            pkg("bibata", json!({"kind": "url", "url": "https://example/Bibata.tar.gz"}), "pinned"),
            json!({"share": share}),
        )
    };

    let refused = w.plan(&[bibata(json!({"icons": "Bibata-Modern-Classic"}))]);
    assert!(refused.unsupported[0].contains("must name an entry inside a directory"), "{:?}", refused.unsupported);

    w.apply(&[bibata(json!({"icons/Bibata-Modern-Classic": "Bibata-Modern-Classic"}))]).unwrap();
    let link = w.paths.system("/usr/local/share/icons/Bibata-Modern-Classic");
    let target = fs::read_link(&link).unwrap();
    assert!(target.starts_with("/opt/fspec/store/"), "{}", target.display());
    assert!(target.to_string_lossy().ends_with("-bibata-2.0.7/Bibata-Modern-Classic"), "{}", target.display());
    assert!(w.plan(&[bibata(json!({"icons/Bibata-Modern-Classic": "Bibata-Modern-Classic"}))]).changes.is_empty());

    fs::remove_file(&link).unwrap();
    let drift = w.plan(&[bibata(json!({"icons/Bibata-Modern-Classic": "Bibata-Modern-Classic"}))]);
    assert_eq!(drift.changes.len(), 1, "{drift:?}");

    w.apply(&[]).unwrap();
    assert!(fs::symlink_metadata(&link).is_err());

    fs::create_dir_all(&link).unwrap();
    let err = w.apply(&[bibata(json!({"icons/Bibata-Modern-Classic": "Bibata-Modern-Classic"}))]).unwrap_err();
    assert!(format!("{err:#}").contains("is not a link fspec made"), "{err:#}");
}

struct Shared(&'static RecordingRunner);

impl Runner for Shared {
    fn run(&self, cmd: &Cmd) -> anyhow::Result<Output> {
        self.0.run(cmd)
    }
}

#[test]
fn etc_links_config_and_units_and_reloads_systemd_for_a_unit() {
    let mut w = World::new();
    let fedora = serde_json::from_value(json!({"kind": "distro", "distro": "fedora", "init": "systemd"})).unwrap();
    w.facts = Facts::declared("m", "x86_64-linux", Some(&fedora)).unwrap();
    let rec: &'static RecordingRunner = Box::leak(Box::default());
    w.runner = Box::new(Shared(rec));
    let body = tgz(&[
        ("usr/bin/asusd", "#!/bin/sh\n", 0o755),
        ("usr/share/dbus-1/system.d/asusd.conf", "<busconfig/>", 0o644),
        ("usr/lib/systemd/system/asusd.service", "[Service]\n", 0o644),
    ]);
    let mut l = locked("asusd", "url:https://example/asusd.tgz", "6.5.0", "https://example/asusd.tgz", &body);
    l.artifacts = BTreeMap::from([("x86_64-linux".to_string(), l.artifacts.remove(PLATFORM).unwrap())]);
    w.lock.upsert(l);
    w.fetch = MapFetch::default().with("https://example/asusd.tgz", body);
    let asusd = |etc: serde_json::Value| {
        with_fields(
            pkg("asusd", json!({"kind": "url", "url": "https://example/asusd.tgz"}), "pinned"),
            json!({"etc": etc}),
        )
    };
    let conf = json!({"dbus-1/system.d/asusd.conf": "usr/share/dbus-1/system.d/asusd.conf"});
    w.apply(&[asusd(conf.clone())]).unwrap();
    let link = w.paths.system("/etc/dbus-1/system.d/asusd.conf");
    assert!(fs::read_link(&link).unwrap().starts_with("/opt/fspec/store/"));
    assert!(!rec.lines().iter().any(|l| l.contains("daemon-reload")));

    let unit = json!({"dbus-1/system.d/asusd.conf": "usr/share/dbus-1/system.d/asusd.conf",
        "systemd/system/asusd.service": "usr/lib/systemd/system/asusd.service"});
    w.apply(&[asusd(unit.clone())]).unwrap();
    assert!(is_link(&w.paths.system("/etc/systemd/system/asusd.service")));
    assert_eq!(rec.lines().iter().filter(|l| *l == "systemctl daemon-reload").count(), 1);

    fs::remove_file(&link).unwrap();
    assert_eq!(w.plan(&[asusd(unit)]).changes.len(), 1);
    w.apply(&[]).unwrap();
    assert!(fs::symlink_metadata(&link).is_err());
    assert!(fs::symlink_metadata(w.paths.system("/etc/systemd/system/asusd.service")).is_err());

    let bad = w.plan(&[asusd(json!({"../passwd": "usr/bin/asusd"}))]);
    assert!(bad.unsupported[0].contains("must be paths under /etc"), "{:?}", bad.unsupported);
}

fn is_link(p: &std::path::Path) -> bool {
    fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink())
}

#[test]
fn apps_are_copied_to_applications_and_go_with_the_package() {
    let mut w = World::new();
    let body = tgz(&[
        ("Tool.app/Contents/Info.plist", "<plist/>", 0o644),
        ("Tool.app/Contents/MacOS/tool", "#!/bin/sh\n", 0o755),
    ]);
    w.lock.upsert(locked("tool", "url:https://example/Tool.tgz", "1.0.0", "https://example/Tool.tgz", &body));
    w.fetch = MapFetch::default().with("https://example/Tool.tgz", body);
    let tool = |apps: serde_json::Value| {
        with_fields(
            pkg("tool", json!({"kind": "url", "url": "https://example/Tool.tgz"}), "pinned"),
            json!({"apps": apps}),
        )
    };
    let refused = w.plan(&[tool(json!(["Tool"]))]);
    assert!(refused.unsupported[0].contains("must be the path of a .app bundle"), "{:?}", refused.unsupported);

    w.apply(&[tool(json!(["Tool.app"]))]).unwrap();
    let app = w.paths.system("/Applications/Tool.app");
    assert!(app.join("Contents/MacOS/tool").is_file());
    assert!(!fs::symlink_metadata(&app).unwrap().file_type().is_symlink());
    assert!(w.plan(&[tool(json!(["Tool.app"]))]).changes.is_empty());

    fs::remove_dir_all(&app).unwrap();
    assert_eq!(w.plan(&[tool(json!(["Tool.app"]))]).changes.len(), 1);
    w.apply(&[tool(json!(["Tool.app"]))]).unwrap();
    assert!(app.is_dir());

    w.apply(&[]).unwrap();
    assert!(fs::symlink_metadata(&app).is_err());

    fs::create_dir_all(&app).unwrap();
    let err = w.apply(&[tool(json!(["Tool.app"]))]).unwrap_err();
    assert!(format!("{err:#}").contains("fspec did not put it there"), "{err:#}");
    assert!(app.is_dir());
}

#[test]
fn npm_without_dependencies_links_its_bin() {
    let mut w = World::new();
    let body = tgz(&[
        ("package/package.json", r#"{"name": "@scope/tool", "bin": "./cli.sh"}"#, 0o644),
        ("package/cli.sh", "#!/bin/sh\necho npm tool\n", 0o644),
    ]);
    w.lock.upsert(locked("tool", "npm:@scope/tool", "1.2.3", "https://example/tool.tgz", &body));
    w.fetch = MapFetch::default().with("https://example/tool.tgz", body);
    w.apply(&[pkg("tool", json!({"kind": "npm", "name": "@scope/tool"}), "pinned")]).unwrap();
    assert_eq!(w.run("tool"), "npm tool");
}

/// Stands in for npm: `npm install --omit=dev --prefix <dir> <tarball>` writes a fake tree.
struct FakeNpm {
    calls: Mutex<Vec<String>>,
}

impl Runner for FakeNpm {
    fn run(&self, cmd: &Cmd) -> anyhow::Result<Output> {
        self.calls.lock().unwrap().push(cmd.to_string());
        let prefix = Path::new(&cmd.args[3]);
        let bin = prefix.join("node_modules/.bin");
        fs::create_dir_all(&bin)?;
        fs::create_dir_all(prefix.join("node_modules/@scope/tool"))?;
        fs::write(prefix.join("node_modules/@scope/tool/package.json"), r#"{"bin": {"tool": "cli.js"}}"#)?;
        fs::write(bin.join("tool"), "#!/bin/sh\necho installed\n")?;
        fs::set_permissions(bin.join("tool"), fs::Permissions::from_mode(0o755))?;
        Ok(Output::default())
    }
}

#[test]
fn npm_with_dependencies_runs_npm_install() {
    let mut w = World::new();
    let body = tgz(&[(
        "package/package.json",
        r#"{"name": "@scope/tool", "bin": {"tool": "cli.js"}, "dependencies": {"left-pad": "1.0.0"}}"#,
        0o644,
    )]);
    w.lock.upsert(locked("tool", "npm:@scope/tool", "1.2.3", "https://example/tool.tgz", &body));
    w.fetch = MapFetch::default().with("https://example/tool.tgz", body);
    let npm = FakeNpm { calls: Mutex::new(vec![]) };
    w.runner = Box::new(npm);
    w.apply(&[pkg("tool", json!({"kind": "npm", "name": "@scope/tool"}), "pinned")]).unwrap();
    assert_eq!(w.run("tool"), "installed");
}

#[test]
fn nix_builds_through_the_runner_and_links_its_bin() {
    let mut w = World::new();
    let out = w.paths.root.join("nixstore/hello-2.12");
    fs::create_dir_all(out.join("bin")).unwrap();
    fs::write(out.join("bin/hello"), "#!/bin/sh\necho nix hello\n").unwrap();
    fs::set_permissions(out.join("bin/hello"), fs::Permissions::from_mode(0o755)).unwrap();
    let runner = RecordingRunner::default();
    runner.respond("nix build", Output { status: 0, stdout: format!("{}\n", out.display()), stderr: String::new() });
    w.runner = Box::new(runner);
    w.lock.upsert(LockedPkg {
        name: "hello".into(),
        source: "nix:hello".into(),
        version: Some("2.12".into()),
        rev: None,
        patches: None,
        parts: Default::default(),
        artifacts: BTreeMap::new(),
    });
    w.apply(&[pkg("hello", json!({"kind": "nix", "attr": "hello"}), "pinned")]).unwrap();
    assert_eq!(w.run("hello"), "nix hello");
    assert_eq!(fs::read_link(w.root().join("current/bin/hello")).unwrap(), out.join("bin/hello"));
}

fn with_data(d: Desired, data: &[&str]) -> Desired {
    let mut value = serde_json::to_value(&d.resource).unwrap();
    value["data"] = json!(data);
    Desired { resource: serde_json::from_value(value).unwrap(), owner: None }
}

impl World {
    fn add_tool(&mut self, name: &str) {
        let url = format!("https://example/{name}.tar.gz");
        let body = tgz(&[(&format!("{name}/{name}"), "#!/bin/sh\necho tool\n", 0o755)]);
        self.lock.upsert(locked(name, &format!("aqua:{name}"), "1.0.0", &url, &body));
        self.fetch = std::mem::take(&mut self.fetch).with(&url, body);
    }

    fn rollback_to(&mut self, n: u64) {
        let dir = self.state_dir();
        let past = State::load(&dir.join(format!("generations/{n}.json"))).unwrap();
        let desired = desired_from_generation(&dir, n).unwrap();
        self.lock = lock_from_state(&past);
        self.fetch = MapFetch::default();
        self.apply(&desired).unwrap();
    }
}

#[test]
fn rollback_brings_a_removed_packages_data_back() {
    let mut w = World::new();
    w.add_tool("extra");
    let data = w.paths.system("/var/lib/extra");
    fs::create_dir_all(&data).unwrap();
    fs::write(data.join("db"), "rows").unwrap();
    let want = [with_data(aqua("extra"), &["/var/lib/extra"])];

    w.apply(&want).unwrap();
    w.apply(&[]).unwrap();
    assert!(!data.exists());
    assert!(w.state_dir().join("trash/2/extra/0-extra/db").exists());

    w.rollback_to(1);
    assert_eq!(fs::read_to_string(data.join("db")).unwrap(), "rows");
    assert_eq!(w.run("extra"), "tool");
}

#[test]
fn gc_keeps_what_current_references_and_deletes_the_rest() {
    let mut w = World::new();
    w.add_tool("extra");
    let data = w.paths.system("/var/lib/extra");
    fs::create_dir_all(&data).unwrap();
    fs::write(data.join("db"), "rows").unwrap();
    let extra = with_data(aqua("extra"), &["/var/lib/extra"]);

    w.add_hello("1.0.0");
    w.apply(&[aqua("hello"), extra]).unwrap();
    w.add_hello("2.0.0");
    w.apply(&[aqua("hello")]).unwrap();
    w.add_hello("3.0.0");
    w.apply(&[aqua("hello")]).unwrap();
    let dir = w.state_dir();
    let now = std::time::SystemTime::now();
    let day = std::time::Duration::from_secs(86_400);

    let none = fspec_store::gc::run(&w.paths, Scope::System, now - 14 * day).unwrap();
    assert!(none.removed.is_empty(), "{none:?}");

    let old = fs::File::open(dir.join("generations/1.json")).unwrap();
    old.set_modified(now - 30 * day).unwrap();
    let some = fspec_store::gc::run(&w.paths, Scope::System, now - 14 * day).unwrap();
    assert_eq!(some.removed.len(), 1, "{some:?}");
    assert!(!dir.join("generations/1.json").exists());
    assert!(dir.join("generations/2.json").exists());

    let report = fspec_store::gc::run(&w.paths, Scope::System, now + day).unwrap();
    assert!(report.freed > 0);
    assert!(!dir.join("generations/2.json").exists());
    assert!(dir.join("generations/3.json").exists());
    assert!(!dir.join("trash/2").exists(), "{:?}", report.removed);
    assert!(!w.root().join("generations/1").exists());
    assert!(!w.root().join("generations/2").exists());
    assert_eq!(w.current(), Path::new("generations/3"));
    assert_eq!(w.run("hello"), "3.0.0");
    // hello 3.0.0 and its sidecar are all that is left in the store.
    assert_eq!(w.store_entries(), 2);
    assert!(w.plan(&[aqua("hello")]).changes.is_empty());
}

#[test]
fn durations_parse() {
    use fspec_store::gc::parse_duration;
    assert_eq!(parse_duration("14d").unwrap().as_secs(), 14 * 86_400);
    assert_eq!(parse_duration("12h").unwrap().as_secs(), 43_200);
    assert_eq!(parse_duration("0s").unwrap().as_secs(), 0);
    assert!(parse_duration("14").is_err());
    assert!(parse_duration("d").is_err());
}

const NPM_PATCH: &str = "\
diff --git a/cli.sh b/cli.sh
--- a/cli.sh
+++ b/cli.sh
@@ -1,2 +1,2 @@
 #!/bin/sh
-echo npm tool
+echo patched tool
";

fn npm_tool() -> Vec<u8> {
    tgz(&[
        ("package/package.json", r#"{"name": "@scope/tool", "bin": "./cli.sh"}"#, 0o644),
        ("package/cli.sh", "#!/bin/sh\necho npm tool\n", 0o644),
    ])
}

fn npm_pkg(patches: &[&str]) -> Desired {
    let resource = serde_json::from_value(json!({
        "kind": "pkg", "name": "tool", "source": {"kind": "npm", "name": "@scope/tool"}, "patches": patches,
    }));
    Desired { resource: resource.unwrap(), owner: None }
}

fn lock_npm(w: &mut World, patches: Option<String>) {
    let body = npm_tool();
    let mut l = locked("tool", "npm:@scope/tool", "1.2.3", "https://example/tool.tgz", &body);
    l.patches = patches;
    w.lock.upsert(l);
    w.fetch = MapFetch::default().with("https://example/tool.tgz", body);
}

fn write_patch(w: &World, text: &str) -> String {
    fs::create_dir_all(w.paths.root.join("patches")).unwrap();
    fs::write(w.paths.root.join("patches/fix.patch"), text).unwrap();
    "patches/fix.patch".into()
}

#[test]
fn npm_patches_apply_to_the_unpacked_tree_and_change_the_store_path() {
    let mut w = World::new();
    lock_npm(&mut w, None);
    w.apply(&[npm_pkg(&[])]).unwrap();
    assert_eq!(w.run("tool"), "npm tool");
    let plain = w.state().entries[&npm_pkg(&[]).resource.key()].record["store"].as_str().unwrap().to_string();

    let patch = write_patch(&w, NPM_PATCH);
    let hash = fspec_core::patches::hash(&w.paths.root, std::slice::from_ref(&patch)).unwrap();
    lock_npm(&mut w, Some(hash));
    let want = [npm_pkg(&[&patch])];
    assert_eq!(w.plan(&want).changes.len(), 1);
    w.apply(&want).unwrap();
    assert_eq!(w.run("tool"), "patched tool");
    let patched = w.state().entries[&want[0].resource.key()].record["store"].as_str().unwrap().to_string();
    assert_ne!(plain, patched);
    assert!(w.root().join(&plain).exists(), "the unpatched tree stays for rollback");
    assert!(w.plan(&want).changes.is_empty());
}

#[test]
fn a_patched_pkg_whose_patches_differ_from_the_lock_is_refused() {
    let mut w = World::new();
    let patch = write_patch(&w, NPM_PATCH);
    let hash = fspec_core::patches::hash(&w.paths.root, std::slice::from_ref(&patch)).unwrap();
    lock_npm(&mut w, Some(hash));
    write_patch(&w, &NPM_PATCH.replace("patched", "edited"));
    let err = w.apply(&[npm_pkg(&[&patch])]).unwrap_err();
    assert!(format!("{err:#}").contains("patches changed, run fspec update"), "{err:#}");
    assert_eq!(w.store_entries(), 0);

    lock_npm(&mut w, None);
    let err = w.apply(&[npm_pkg(&[&patch])]).unwrap_err();
    assert!(format!("{err:#}").contains("patches changed, run fspec update"), "{err:#}");
}

#[test]
fn a_patch_that_does_not_apply_aborts_the_apply() {
    let mut w = World::new();
    let patch = write_patch(&w, &NPM_PATCH.replace("echo npm tool", "echo something else"));
    let hash = fspec_core::patches::hash(&w.paths.root, std::slice::from_ref(&patch)).unwrap();
    lock_npm(&mut w, Some(hash));
    let err = w.apply(&[npm_pkg(&[&patch])]).unwrap_err();
    assert!(format!("{err:#}").contains("patching tool"), "{err:#}");
    assert_eq!(w.store_entries(), 0);
}

const REF: &str = "ghcr.io/o/fspec-cache/tool@sha256:aa";

fn tar_zst(name: &str, content: &str) -> Vec<u8> {
    let mut tar = tar::Builder::new(Vec::new());
    let mut h = tar::Header::new_gnu();
    h.set_size(content.len() as u64);
    h.set_mode(0o755);
    tar.append_data(&mut h, name, content.as_bytes()).unwrap();
    let tar = tar.into_inner().unwrap();
    ruzstd::encoding::compress_to_vec(tar.as_slice(), ruzstd::encoding::CompressionLevel::Fastest)
}

/// Plays `oras`: `pull` drops the artifact into the `-o` directory, `manifest fetch` names its layer.
struct FakeOras {
    artifact: Vec<u8>,
    layer_digest: String,
    calls: Mutex<Vec<String>>,
}

impl FakeOras {
    fn new(artifact: Vec<u8>) -> FakeOras {
        let layer_digest = format!("sha256:{}", sha256_hex(&artifact));
        FakeOras { artifact, layer_digest, calls: Mutex::new(vec![]) }
    }
}

fn sha256_hex(body: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(body).iter().map(|b| format!("{b:02x}")).collect()
}

impl Runner for FakeOras {
    fn run(&self, cmd: &Cmd) -> anyhow::Result<Output> {
        self.calls.lock().unwrap().push(cmd.to_string());
        let mut out = Output::default();
        match cmd.args[0].as_str() {
            "pull" => {
                assert_eq!(cmd.args[1], REF);
                assert_eq!(cmd.args[2], "-o");
                fs::write(Path::new(&cmd.args[3]).join("tool-aarch64-darwin.tar.zst"), &self.artifact)?;
            }
            "manifest" => {
                out.stdout = json!({"layers": [{
                    "digest": self.layer_digest,
                    "annotations": {"org.opencontainers.image.title": "tool-aarch64-darwin.tar.zst"},
                }]})
                .to_string();
            }
            other => panic!("unexpected oras {other}"),
        }
        Ok(out)
    }
}

fn git_pkg() -> Desired {
    pkg("tool", json!({"kind": "git", "url": "https://example/tool", "rev": "main"}), "pinned")
}

fn lock_git(w: &mut World, reference: &str) {
    w.lock.upsert(LockedPkg {
        name: "tool".into(),
        source: "git:https://example/tool@main".into(),
        version: None,
        rev: Some("4f1c2a9d3e5b7a8c9d0e1f2a3b4c5d6e7f8a9b0c".into()),
        patches: None,
        parts: Default::default(),
        artifacts: BTreeMap::from([(
            PLATFORM.to_string(),
            Artifact { oci: Some(reference.into()), ..Default::default() },
        )]),
    });
}

#[test]
fn a_ci_built_package_is_pulled_with_oras_and_installed() {
    let mut w = World::new();
    lock_git(&mut w, REF);
    let oras = std::sync::Arc::new(FakeOras::new(tar_zst("tool", "#!/bin/sh\necho from ci\n")));
    struct Shared(std::sync::Arc<FakeOras>);
    impl Runner for Shared {
        fn run(&self, cmd: &Cmd) -> anyhow::Result<Output> {
            self.0.run(cmd)
        }
    }
    w.runner = Box::new(Shared(oras.clone()));
    w.apply(&[git_pkg()]).unwrap();
    assert_eq!(w.run("tool"), "from ci");

    let calls = oras.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert!(calls[0].starts_with(&format!("oras pull {REF} -o ")), "{calls:?}");
    assert_eq!(calls[1], format!("oras manifest fetch {REF}"));
    let record = &w.state().entries[&git_pkg().resource.key()].record;
    assert_eq!(record["id"], REF);
    assert_eq!(record["bins"]["tool"], "tool");
}

#[test]
fn an_oci_layer_that_does_not_match_its_manifest_is_refused() {
    let mut w = World::new();
    lock_git(&mut w, REF);
    let mut oras = FakeOras::new(tar_zst("tool", "#!/bin/sh\necho from ci\n"));
    oras.layer_digest = "sha256:00".into();
    w.runner = Box::new(oras);
    let err = w.apply(&[git_pkg()]).unwrap_err();
    assert!(format!("{err:#}").contains("manifest says"), "{err:#}");
    assert_eq!(w.store_entries(), 0);
}

#[test]
fn a_tag_without_a_digest_is_refused_and_never_pulled() {
    let mut w = World::new();
    lock_git(&mut w, "ghcr.io/o/fspec-cache/tool:abc-aarch64-darwin");
    let runner = RecordingRunner::default();
    w.runner = Box::new(runner);
    let err = w.apply(&[git_pkg()]).unwrap_err();
    assert!(format!("{err:#}").contains("not pinned to a digest"), "{err:#}");
}

/// Runs the recipe's own `sh -c` for real, so the build leaves an executable; git and the rest
/// answer like a clean checkout.
struct BuildHere;

impl Runner for BuildHere {
    fn run(&self, cmd: &Cmd) -> anyhow::Result<Output> {
        if cmd.program != "sh" {
            return Ok(Output::default());
        }
        let out = std::process::Command::new("sh").args(&cmd.args).envs(cmd.env.iter().cloned()).output()?;
        Ok(Output { status: out.status.code().unwrap_or(1), ..Output::default() })
    }
}

#[test]
fn a_git_package_without_a_ci_artifact_is_built_on_the_host() {
    let mut w = World::new();
    w.lock.upsert(LockedPkg {
        name: "tool".into(),
        source: "git:https://example/tool@main".into(),
        version: None,
        rev: Some("4f1c2a9d3e5b7a8c9d0e1f2a3b4c5d6e7f8a9b0c".into()),
        patches: None,
        parts: Default::default(),
        artifacts: BTreeMap::new(),
    });
    w.runner = Box::new(BuildHere);
    let script = r#"mkdir -p "$FSPEC_OUT/bin" && printf '#!/bin/sh\necho built here\n' > "$FSPEC_OUT/bin/tool" && chmod +x "$FSPEC_OUT/bin/tool""#;
    let resource = serde_json::from_value(json!({"kind": "pkg", "name": "tool", "mode": "pinned",
        "source": {"kind": "git", "url": "https://example/tool", "rev": "main"}, "build": ["sh", "-c", script]}));
    w.apply(&[Desired { resource: resource.unwrap(), owner: None }]).unwrap();
    assert_eq!(w.run("tool"), "built here");
    assert!(w.root().join("current/pkgs/tool/bin/tool").is_file());

    // A build that installs only a library is reachable through current/pkgs.
    let lib = r#"mkdir -p "$FSPEC_OUT/lib" && echo so > "$FSPEC_OUT/lib/pam_x.so""#;
    let resource = serde_json::from_value(json!({"kind": "pkg", "name": "tool", "mode": "pinned",
        "source": {"kind": "git", "url": "https://example/tool", "rev": "main"}, "build": ["sh", "-c", lib]}));
    w.apply(&[Desired { resource: resource.unwrap(), owner: None }]).unwrap();
    assert_eq!(fs::read_to_string(w.root().join("current/pkgs/tool/lib/pam_x.so")).unwrap(), "so\n");
    assert!(!w.root().join("current/bin/tool").exists());
}

#[test]
fn parts_unpack_into_the_package_root_at_their_locked_hash() {
    let mut w = World::new();
    let body = tgz(&[("files/discord", "#!/bin/sh\n", 0o755)]);
    let core = tgz(&[("index.js", "module.exports = 1\n", 0o644)]);
    let (main, part) = ("https://example/discord.tgz", "https://example/core.tgz");
    let mut l = locked("discord", &format!("url:{main}"), "1.0.160", main, &body);
    l.parts.insert(
        "modules/core".into(),
        Artifact { url: Some(part.into()), hash: Some(sri_sha256(&core)), ..Default::default() },
    );
    w.lock.upsert(l);
    w.fetch = MapFetch::default().with(main, body).with(part, core);
    let discord = with_fields(
        pkg("discord", json!({"kind": "url", "url": main}), "pinned"),
        json!({"parts": {"modules/core": {"kind": "url", "url": part}}}),
    );
    w.apply(std::slice::from_ref(&discord)).unwrap();
    let js = w.root().join("current/pkgs/discord/files/modules/core/index.js");
    assert_eq!(fs::read_to_string(js).unwrap(), "module.exports = 1\n");

    let moved =
        with_fields(discord, json!({"parts": {"modules/core": {"kind": "url", "url": "https://example/other.tgz"}}}));
    let err = format!("{:#}", w.apply(&[moved]).unwrap_err());
    assert!(err.contains("no hash for https://example/other.tgz"), "{err}");
}

/// Counts the downloads in flight, to show prepare runs them at once.
struct Overlapping {
    inner: MapFetch,
    now: std::sync::atomic::AtomicUsize,
    most: std::sync::atomic::AtomicUsize,
}

impl fspec_core::fetch::Fetch for Overlapping {
    fn get(&self, url: &str) -> anyhow::Result<Vec<u8>> {
        use std::sync::atomic::Ordering::SeqCst;
        self.most.fetch_max(self.now.fetch_add(1, SeqCst) + 1, SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(20));
        self.now.fetch_sub(1, SeqCst);
        self.inner.get(url)
    }
}

#[test]
fn prepare_downloads_packages_at_once() {
    let mut w = World::new();
    let names = ["a", "b", "c", "d"];
    for n in names {
        let url = format!("https://example/{n}.tar.gz");
        let body = tgz(&[(&format!("{n}-1/{n}"), "#!/bin/sh\n", 0o755)]);
        w.lock.upsert(locked(n, &format!("aqua:{n}"), "1", &url, &body));
        w.fetch = std::mem::take(&mut w.fetch).with(&url, body);
    }
    let fetch = Overlapping { inner: std::mem::take(&mut w.fetch), now: Default::default(), most: Default::default() };
    let cx = Cx { fetch: &fetch, ..w.cx() };
    let want: Vec<Desired> = names.iter().map(|n| aqua(n)).collect();
    apply_scope(&cx, &[&StoreBackend as &dyn Backend], &want).unwrap();
    assert!(names.iter().all(|n| w.root().join("current/bin").join(n).exists()));
    assert!(fetch.most.load(std::sync::atomic::Ordering::SeqCst) > 1);
}
