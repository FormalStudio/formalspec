use std::fs;

use fspec_core::apply::{Backend, Cx, apply_scope, plan_scope, state_dir};
use fspec_core::facts::Facts;
use fspec_core::fetch::MapFetch;
use fspec_core::lock::{Artifact, Lock, LockedPkg, LockedZypper};
use fspec_core::plan::{Action, Plan};
use fspec_core::resource::{Desired, Distro, Image, Resource, Scope, System};
use fspec_core::runner::{Output, RecordingRunner};
use fspec_core::{Paths, State};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    AptBackend, BrewBackend, DistroBackend, DnfBackend, FlatpakBackend, MossBackend, PacmanBackend, XbpsBackend,
    ZypperBackend,
};

const ALL: [&dyn Backend; 9] = [
    &FlatpakBackend,
    &BrewBackend,
    &DnfBackend,
    &PacmanBackend,
    &AptBackend,
    &XbpsBackend,
    &MossBackend,
    &ZypperBackend,
    &DistroBackend,
];

struct World {
    _dir: tempfile::TempDir,
    paths: Paths,
    facts: Facts,
    lock: Lock,
    runner: RecordingRunner,
    fetch: MapFetch,
}

impl World {
    fn new(platform: &str, system: Option<System>) -> World {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().into(), dir.path().join("home/u"));
        let facts = Facts::declared("h", platform, system.as_ref()).unwrap();
        World {
            _dir: dir,
            paths,
            facts,
            lock: Lock::default(),
            runner: RecordingRunner::default(),
            fetch: MapFetch::default(),
        }
    }

    fn linux() -> World {
        World::on("fedora", &[])
    }

    fn on(distro: &str, packages: &[&str]) -> World {
        let packages = packages.iter().map(ToString::to_string).collect();
        let tumbleweed = distro == TW;
        let distro =
            Distro { distro: distro.into(), init: fspec_core::facts::Init::Systemd, packages, kernel_args: vec![] };
        let mut w = World::new("x86_64-linux", Some(System::Distro(distro)));
        if tumbleweed {
            w.lock.zypper.insert("h".into(), LockedZypper { snapshot: "20261018".into() });
        }
        w
    }

    fn darwin() -> World {
        World::new("aarch64-darwin", None)
    }

    fn cx(&self, scope: Scope) -> Cx<'_> {
        let generation = self.state(scope).generation + 1;
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
            checkout: None,
        }
    }

    fn state(&self, scope: Scope) -> State {
        State::load(&state_dir(&self.paths, scope).join("state.json")).unwrap()
    }

    fn apply(&self, scope: Scope, want: &[Desired]) -> anyhow::Result<Plan> {
        apply_scope(&self.cx(scope), &ALL, want)
    }

    fn plan(&self, scope: Scope, want: &[Desired]) -> anyhow::Result<Plan> {
        let state = self.state(scope);
        plan_scope(&self.cx(scope), &ALL, want, &state)
    }

    /// Commands since the last call.
    fn take(&self) -> Vec<String> {
        let lines = self.runner.lines();
        self.runner.calls.lock().unwrap().clear();
        lines
    }

    fn answer(&self, prefix: &str, status: i32, stdout: &str) {
        let out = Output { status, stdout: stdout.into(), stderr: String::new() };
        self.runner.responses.lock().unwrap().insert(0, (prefix.into(), out));
    }

    fn forget_answers(&self) {
        self.runner.responses.lock().unwrap().clear();
    }
}

fn pkg(name: &str, source: Value, mode: &str) -> Desired {
    let resource = json!({"kind": "pkg", "name": name, "source": source, "mode": mode});
    Desired { resource: serde_json::from_value::<Resource>(resource).unwrap(), owner: None }
}

fn flatpak(name: &str, id: &str) -> Desired {
    pkg(name, json!({"kind": "flatpak", "id": id}), "pinned")
}

fn locked(name: &str, source: &str, version: Option<&str>, artifacts: &[(&str, Artifact)]) -> LockedPkg {
    LockedPkg {
        name: name.into(),
        source: source.into(),
        version: version.map(Into::into),
        rev: None,
        patches: None,
        parts: Default::default(),
        artifacts: artifacts.iter().map(|(k, a)| (k.to_string(), a.clone())).collect(),
    }
}

fn commit(c: &str) -> Artifact {
    Artifact { commit: Some(c.into()), ..Default::default() }
}

fn sri(bytes: &[u8]) -> String {
    use base64::Engine;
    format!("sha256-{}", base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes)))
}

fn is_drift(plan: &Plan) -> bool {
    plan.changes.iter().any(|c| matches!(c.action, Action::Drift { .. }))
}

const INSTALL: &str = "flatpak install -y --noninteractive --system flathub";
const PIN: &str = "flatpak update -y --noninteractive --system";
const UNINSTALL: &str = "flatpak uninstall -y --noninteractive --system";
const INFO: &str = "flatpak info --system --show-commit";

#[test]
fn flatpak_install_pins_and_converges() {
    let mut w = World::linux();
    w.lock.upsert(locked("firefox", "flatpak:org.mozilla.firefox", None, &[("any", commit("aaa"))]));
    let want = [flatpak("firefox", "org.mozilla.firefox")];

    w.apply(Scope::System, &want).unwrap();
    assert_eq!(
        w.take(),
        [format!("{INSTALL} org.mozilla.firefox"), format!("{PIN} --commit=aaa org.mozilla.firefox"),]
    );
    assert_eq!(w.state(Scope::System).entries[&want[0].resource.key()].record["commit"], "aaa");

    w.answer(INFO, 0, "aaa\n");
    assert!(w.plan(Scope::System, &want).unwrap().changes.is_empty());
    w.take();

    w.forget_answers();
    w.answer(INFO, 0, "zzz\n");
    let plan = w.plan(Scope::System, &want).unwrap();
    assert!(is_drift(&plan), "{plan}");
    w.apply(Scope::System, &want).unwrap();
    assert_eq!(w.take().last().unwrap(), &format!("{PIN} --commit=aaa org.mozilla.firefox"));
}

#[test]
fn flatpak_lock_bump_repins_and_missing_commit_fails() {
    let mut w = World::linux();
    w.lock.upsert(locked("firefox", "flatpak:org.mozilla.firefox", None, &[("any", commit("aaa"))]));
    let want = [flatpak("firefox", "org.mozilla.firefox")];
    w.apply(Scope::System, &want).unwrap();
    w.take();

    w.lock.upsert(locked("firefox", "flatpak:org.mozilla.firefox", None, &[("any", commit("bbb"))]));
    w.answer(INFO, 0, "aaa\n");
    assert!(is_drift(&w.plan(Scope::System, &want).unwrap()));
    w.apply(Scope::System, &want).unwrap();
    let lines = w.take();
    assert_eq!(lines.last().unwrap(), &format!("{PIN} --commit=bbb org.mozilla.firefox"));
    assert_eq!(w.state(Scope::System).entries[&want[0].resource.key()].record["commit"], "bbb");

    let bare = World::linux();
    let plan = bare.plan(Scope::System, &want).unwrap();
    assert!(plan.unsupported[0].contains("fspec update"), "{:?}", plan.unsupported);
}

#[test]
fn flatpak_self_managed_installs_once() {
    let mut w = World::linux();
    w.lock.upsert(locked("steam", "flatpak:com.valvesoftware.Steam", None, &[("any", commit("aaa"))]));
    let want = [pkg("steam", json!({"kind": "flatpak", "id": "com.valvesoftware.Steam"}), "self_managed")];
    w.apply(Scope::System, &want).unwrap();
    assert!(!w.take().iter().any(|l| l.starts_with(PIN)));

    // The app updated itself and the lock moved on: nothing to do.
    w.lock.upsert(locked("steam", "flatpak:com.valvesoftware.Steam", None, &[("any", commit("bbb"))]));
    w.answer(INFO, 0, "ccc\n");
    assert!(w.plan(Scope::System, &want).unwrap().changes.is_empty());
}

#[test]
fn flatpak_removal_uninstalls() {
    let mut w = World::linux();
    w.lock.upsert(locked("firefox", "flatpak:org.mozilla.firefox", None, &[("any", commit("aaa"))]));
    let want = [flatpak("firefox", "org.mozilla.firefox")];
    w.apply(Scope::System, &want).unwrap();
    w.take();

    w.apply(Scope::System, &[]).unwrap();
    assert_eq!(w.take(), [format!("{INFO} org.mozilla.firefox"), format!("{UNINSTALL} org.mozilla.firefox")]);
    assert!(w.state(Scope::System).entries.is_empty());
}

#[test]
fn flatpak_failure_keeps_the_earlier_install() {
    let mut w = World::linux();
    w.lock.upsert(locked("a", "flatpak:org.a", None, &[("any", commit("aaa"))]));
    w.lock.upsert(locked("b", "flatpak:org.b", None, &[("any", commit("bbb"))]));
    w.answer(&format!("{INSTALL} org.b"), 1, "");
    let err = w.apply(Scope::System, &[flatpak("a", "org.a"), flatpak("b", "org.b")]).unwrap_err();
    assert!(format!("{err:#}").contains("rolled back"), "{err:#}");
    assert_eq!(
        w.take(),
        [format!("{INSTALL} org.a"), format!("{PIN} --commit=aaa org.a"), format!("{INSTALL} org.b"),]
    );
    assert!(w.state(Scope::System).entries.is_empty());
}

#[test]
fn flatpak_is_not_claimed_on_darwin() {
    let w = World::darwin();
    let d = flatpak("firefox", "org.mozilla.firefox");
    assert!(!FlatpakBackend.handles(&w.cx(Scope::System), &d.resource));
    assert!(!w.plan(Scope::System, &[d]).unwrap().unsupported.is_empty());
}

fn brew(name: &str, cask: bool, mode: &str) -> Desired {
    pkg(name, json!({"kind": "brew", "cask": cask}), mode)
}

#[test]
fn brew_installs_formula_and_cask_in_user_scope() {
    let mut w = World::darwin();
    w.lock.upsert(locked("ripgrep", "brew", Some("14.1.0"), &[]));
    w.answer("brew list --versions ripgrep", 0, "ripgrep 14.1.0\n");
    let want = [
        brew("ripgrep", false, "pinned"),
        brew("aerospace", true, "pinned"),
        pkg("fd-find", json!({"kind": "brew", "name": "fd"}), "pinned"),
    ];
    assert!(w.plan(Scope::System, &want).unwrap().changes.is_empty());

    w.apply(Scope::User, &want).unwrap();
    assert_eq!(
        w.take(),
        [
            "brew install --cask aerospace",
            "brew list --cask --versions aerospace",
            "brew install fd",
            "brew list --versions fd",
            "brew install ripgrep",
            "brew list --versions ripgrep",
        ]
    );
    let state = w.state(Scope::User);
    assert_eq!(state.entries[&want[0].resource.key()].record["version"], "14.1.0");
    assert!(!state.entries[&want[1].resource.key()].record.contains_key("version"));
    assert!(w.plan(Scope::User, &want).unwrap().changes.is_empty());
}

#[test]
fn a_tapped_brew_package_taps_then_installs_by_its_full_name() {
    let w = World::darwin();
    let want = [
        pkg("sketchybar", json!({"kind": "brew", "tap": "felixkratz/formulae"}), "pinned"),
        pkg("omniwm", json!({"kind": "brew", "cask": true, "tap": "barutsrb/tap"}), "pinned"),
    ];
    w.apply(Scope::User, &want).unwrap();
    assert_eq!(
        w.take(),
        [
            "brew tap barutsrb/tap",
            "brew install --cask barutsrb/tap/omniwm",
            "brew list --cask --versions omniwm",
            "brew tap felixkratz/formulae",
            "brew install felixkratz/formulae/sketchybar",
            "brew list --versions sketchybar",
        ]
    );
    let bad = [pkg("x", json!({"kind": "brew", "tap": "felixkratz"}), "pinned")];
    let err = w.apply(Scope::User, &bad).unwrap_err();
    assert!(format!("{err:#}").contains("must be `owner/repo`"), "{err:#}");
}

#[test]
fn brew_pinned_upgrades_on_drift_and_self_managed_never_does() {
    let mut w = World::darwin();
    w.lock.upsert(locked("ripgrep", "brew", Some("14.1.0"), &[]));
    w.lock.upsert(locked("jq", "brew", Some("1.7.0"), &[]));
    w.answer("brew list --versions ripgrep", 0, "ripgrep 14.1.0\n");
    w.answer("brew list --versions jq", 0, "jq 1.7.0\n");
    let want = [brew("ripgrep", false, "pinned"), brew("jq", false, "self_managed")];
    w.apply(Scope::User, &want).unwrap();
    w.take();

    w.lock.upsert(locked("ripgrep", "brew", Some("15.0.0"), &[]));
    w.lock.upsert(locked("jq", "brew", Some("1.8.0"), &[]));
    let plan = w.plan(Scope::User, &want).unwrap();
    assert_eq!(plan.changes.len(), 1, "{plan}");
    assert!(is_drift(&plan));

    w.apply(Scope::User, &want).unwrap();
    let lines = w.take();
    assert_eq!(lines.iter().filter(|l| l.starts_with("brew upgrade")).collect::<Vec<_>>(), ["brew upgrade ripgrep"]);
    assert_eq!(
        &lines[lines.len() - 3..],
        ["brew list --versions ripgrep", "brew upgrade ripgrep", "brew list --versions ripgrep"]
    );

    // A rebuilt formula reports 14.1.0_1 for upstream 14.1.0.
    w.lock.upsert(locked("ripgrep", "brew", Some("14.1.0"), &[]));
    w.forget_answers();
    w.answer("brew list --versions ripgrep", 0, "ripgrep 14.1.0_1\n");
    assert!(w.plan(Scope::User, &want).unwrap().changes.is_empty());
}

#[test]
fn brew_removal_and_rollback() {
    let w = World::darwin();
    let want = [brew("a", false, "pinned"), brew("b", true, "pinned")];
    w.apply(Scope::User, &want).unwrap();
    w.take();
    w.apply(Scope::User, &want[..1]).unwrap();
    assert_eq!(w.take().last().unwrap(), "brew uninstall --cask b");

    w.answer("brew install --cask c", 1, "");
    let err = w.apply(Scope::User, &[want[0].clone(), brew("c", true, "pinned")]).unwrap_err();
    assert!(format!("{err:#}").contains("rolled back"), "{err:#}");
    w.forget_answers();
    let lines: Vec<_> = w.take().into_iter().filter(|l| !l.starts_with("brew list")).collect();
    assert_eq!(lines, ["brew install --cask c"]);
}

#[test]
fn brew_failure_keeps_the_earlier_install() {
    let w = World::darwin();
    w.answer("brew install b", 1, "");
    let err = w.apply(Scope::User, &[brew("a", false, "pinned"), brew("b", false, "pinned")]).unwrap_err();
    assert!(format!("{err:#}").contains("rolled back"), "{err:#}");
    assert_eq!(w.take(), ["brew install a", "brew list --versions a", "brew install b"]);
}

fn dnf(name: &str, names: &[&str], mode: &str) -> Desired {
    pkg(name, json!({"kind": "dnf", "names": names}), mode)
}

const QF: &str = "rpm -q --whatprovides --qf %{VERSION}-%{RELEASE}\\n";
const BATCH: &str = r"rpm -q --qf %{NAME}\t";
const ALL_AT_3_3: &str = "htop\t3.3-1.fc42\njq\t3.3-1.fc42\nripgrep\t3.3-1.fc42\n";

#[test]
fn dnf_installs_by_name_and_detects_drift() {
    let w = World::linux();
    w.answer(BATCH, 0, ALL_AT_3_3);
    let want = [dnf("htop", &[], "pinned"), dnf("tools", &["jq", "ripgrep"], "pinned")];
    w.apply(Scope::System, &want).unwrap();
    assert_eq!(
        w.take(),
        [
            "dnf install -y --setopt=max_parallel_downloads=10 htop jq ripgrep",
            r"rpm -q --qf %{NAME}\t%{VERSION}-%{RELEASE}\n htop jq ripgrep",
        ]
    );
    assert!(w.plan(Scope::System, &want).unwrap().changes.is_empty());

    w.forget_answers();
    w.answer(BATCH, 0, &ALL_AT_3_3.replace("htop\t3.3", "htop\t3.4"));
    let plan = w.plan(Scope::System, &want).unwrap();
    assert!(is_drift(&plan), "{plan}");

    w.forget_answers();
    w.answer(BATCH, 1, &ALL_AT_3_3.replace("jq\t3.3-1.fc42", "package jq is not installed"));
    w.answer(&format!("{QF} jq"), 1, "package jq is not installed");
    let plan = w.plan(Scope::System, &want).unwrap();
    assert_eq!(plan.changes.len(), 1, "{plan}");

    w.forget_answers();
    w.take();
    w.apply(Scope::System, &[]).unwrap();
    assert_eq!(w.take(), ["dnf remove -y htop", "dnf remove -y jq ripgrep"]);
}

fn rpm_world(body: &[u8], file: &str) -> World {
    let mut w = World::linux();
    let url = format!("https://example/{file}");
    let artifact = Artifact { url: Some(url.clone()), hash: Some(sri(body)), ..Default::default() };
    w.lock.upsert(locked("htop", "dnf:", Some("3.3"), &[("x86_64-linux", artifact)]));
    w.fetch = MapFetch::default().with(&url, body);
    w
}

#[test]
fn dnf_caches_the_locked_rpm_and_installs_the_file() {
    let w = rpm_world(b"rpm one", "htop-3.3-1.fc42.x86_64.rpm");
    let want = [dnf("htop", &[], "pinned")];
    w.apply(Scope::System, &want).unwrap();
    let cached = w.paths.root.join("var/cache/fspec/pkgs/htop-3.3-1.fc42.x86_64.rpm");
    assert_eq!(fs::read(&cached).unwrap(), b"rpm one");
    assert_eq!(w.take()[0], format!("dnf install -y --setopt=max_parallel_downloads=10 {}", cached.display()));
    assert_eq!(w.state(Scope::System).entries[&want[0].resource.key()].record["file"], "htop-3.3-1.fc42.x86_64.rpm");
    assert!(w.plan(Scope::System, &want).unwrap().changes.is_empty());
}

#[test]
fn dnf_lock_bump_installs_the_new_rpm_and_journals_the_old_one() {
    let mut w = rpm_world(b"rpm one", "htop-3.3-1.fc42.x86_64.rpm");
    let want = [dnf("htop", &[], "pinned"), dnf("zzz", &[], "pinned")];
    w.apply(Scope::System, &want[..1]).unwrap();
    w.take();

    let new = Artifact {
        url: Some("https://example/htop-3.4-1.fc42.x86_64.rpm".into()),
        hash: Some(sri(b"rpm two")),
        ..Default::default()
    };
    w.lock.upsert(locked("htop", "dnf:", Some("3.4"), &[("x86_64-linux", new)]));
    w.fetch = MapFetch::default().with("https://example/htop-3.4-1.fc42.x86_64.rpm", &b"rpm two"[..]);
    let plan = w.plan(Scope::System, &want[..1]).unwrap();
    assert!(is_drift(&plan), "{plan}");

    // zzz fails after htop upgraded, so the old cached RPM goes back on.
    w.answer("dnf install -y --setopt=max_parallel_downloads=10 zzz", 1, "");
    assert!(w.apply(Scope::System, &want).is_err());
    let dir = w.paths.root.join("var/cache/fspec/pkgs");
    let lines: Vec<_> = w.take().into_iter().filter(|l| !l.starts_with("rpm -q")).collect();
    assert_eq!(
        lines,
        [
            format!(
                "dnf install -y --setopt=max_parallel_downloads=10 {}",
                dir.join("htop-3.4-1.fc42.x86_64.rpm").display()
            ),
            "dnf install -y --setopt=max_parallel_downloads=10 zzz".to_string(),
            format!(
                "dnf install -y --setopt=max_parallel_downloads=10 {}",
                dir.join("htop-3.3-1.fc42.x86_64.rpm").display()
            ),
        ]
    );
}

#[test]
fn dnf_rejects_a_bad_hash_before_touching_the_system() {
    let mut w = rpm_world(b"rpm one", "htop-3.3-1.fc42.x86_64.rpm");
    w.fetch = MapFetch::default().with("https://example/htop-3.3-1.fc42.x86_64.rpm", &b"tampered"[..]);
    let err = w.apply(Scope::System, &[dnf("htop", &[], "pinned")]).unwrap_err();
    assert!(format!("{err:#}").contains("hashes to"), "{err:#}");
    assert!(w.take().is_empty());
}

#[test]
fn dnf_failure_keeps_the_earlier_install() {
    let w = World::linux();
    w.answer("dnf install -y --setopt=max_parallel_downloads=10 b", 1, "");
    w.answer("dnf install -y --setopt=max_parallel_downloads=10 a b", 1, "");
    assert!(w.apply(Scope::System, &[dnf("a", &[], "pinned"), dnf("b", &[], "pinned")]).is_err());
    let lines = w.take();
    assert_eq!(lines.last().unwrap(), "dnf install -y --setopt=max_parallel_downloads=10 b");
    assert!(!lines.iter().any(|l| l.starts_with("dnf remove")), "{lines:#?}");
}

#[test]
fn dnf_is_left_to_the_image_on_image_hosts() {
    let image = Image {
        base: "quay.io/fedora/fedora-silverblue:42".into(),
        packages: None,
        kernel_args: vec![],
        distro: None,
        ..Default::default()
    };
    let w = World::new("x86_64-linux", Some(System::Image(image)));
    assert_eq!(w.facts.distro, "fedora");
    let d = dnf("htop", &[], "pinned");
    assert!(!DnfBackend.handles(&w.cx(Scope::System), &d.resource));
    assert_eq!(w.plan(Scope::System, &[d]).unwrap().unsupported.len(), 1);

    let arch = World::new(
        "x86_64-linux",
        Some(System::Distro(Distro {
            distro: "arch".into(),
            init: fspec_core::facts::Init::Systemd,
            packages: vec![],
            kernel_args: vec![],
        })),
    );
    assert!(!DnfBackend.handles(&arch.cx(Scope::System), &dnf("htop", &[], "pinned").resource));
}

#[test]
fn flatpak_removal_trashes_the_app_data_under_var_app() {
    let mut w = World::linux();
    w.lock.upsert(locked("firefox", "flatpak:org.mozilla.firefox", None, &[("any", commit("aaa"))]));
    let want = [flatpak("firefox", "org.mozilla.firefox")];
    let data = w.paths.home.join(".var/app/org.mozilla.firefox");
    fs::create_dir_all(data.join("config")).unwrap();
    fs::write(data.join("config/prefs"), "x").unwrap();

    w.apply(Scope::System, &want).unwrap();
    let plan = w.plan(Scope::System, &[]).unwrap();
    assert!(plan.to_string().contains("data to trash: ~/.var/app/org.mozilla.firefox"), "{plan}");
    w.apply(Scope::System, &[]).unwrap();
    assert!(!data.exists());
    let trashed = w.paths.home.join(".local/state/fspec/trash/system-2/firefox/0-org.mozilla.firefox/config/prefs");
    assert!(trashed.exists());

    w.apply(Scope::System, &want).unwrap();
    assert!(data.join("config/prefs").exists());
}

const TW: &str = "opensuse-tumbleweed";

fn is_snapshot_move(line: &str) -> bool {
    ["refresh", "dist-upgrade", "modifyrepo"].iter().any(|c| line.starts_with(&format!("zypper --non-interactive {c}")))
}

/// How each distro family spells its commands.
struct Fam {
    distro: &'static str,
    kind: &'static str,
    install: &'static str,
    remove: &'static str,
    version: &'static str,
    /// The start of the query that asks for many names at once, empty when there is none.
    batch: &'static str,
    /// What the version query prints for a package at 3.3.
    printed: &'static str,
    ext: &'static str,
}

const FAMS: [Fam; 5] = [
    Fam {
        distro: "fedora",
        kind: "dnf",
        install: "dnf install -y --setopt=max_parallel_downloads=10",
        remove: "dnf remove -y",
        version: "rpm -q --whatprovides --qf %{VERSION}-%{RELEASE}\\n",
        batch: "rpm -q --qf %{NAME}\\t",
        printed: "3.3-1",
        ext: "rpm",
    },
    Fam {
        distro: "arch",
        kind: "pacman",
        install: "pacman -S --noconfirm --needed",
        remove: "pacman -R --noconfirm",
        version: "pacman -Q",
        batch: "pacman -Q",
        printed: "htop 3.3-1",
        ext: "pkg.tar.zst",
    },
    Fam {
        distro: "debian",
        kind: "apt",
        install: "apt-get install -y",
        remove: "apt-get remove -y",
        version: "dpkg-query -W -f=${Version}",
        batch: "dpkg-query -W -f=${Package}",
        printed: "3.3-1",
        ext: "deb",
    },
    Fam {
        distro: "void",
        kind: "xbps",
        install: "xbps-install -y",
        remove: "xbps-remove -y",
        version: "xbps-query -p pkgver",
        batch: "",
        printed: "htop-3.3_1",
        ext: "xbps",
    },
    Fam {
        distro: TW,
        kind: "zypper",
        install: "zypper --non-interactive install --auto-agree-with-licenses",
        remove: "zypper --non-interactive remove --clean-deps",
        version: "rpm -q --whatprovides --qf %{VERSION}-%{RELEASE}\\n",
        batch: "rpm -q --qf %{NAME}\\t",
        printed: "3.3-1",
        ext: "rpm",
    },
];

impl Fam {
    fn pkg(&self, name: &str, names: &[&str]) -> Desired {
        pkg(name, json!({"kind": self.kind, "names": names}), "pinned")
    }

    fn world(&self) -> World {
        World::on(self.distro, &[])
    }

    /// The commands that install the cached `file` as `names`.
    fn install_file(&self, w: &World, file: &str, names: &str) -> Vec<String> {
        let dir = w.paths.root.join("var/cache/fspec/pkgs");
        let path = dir.join(file);
        match self.kind {
            "dnf" => vec![format!("dnf install -y --setopt=max_parallel_downloads=10 {}", path.display())],
            "pacman" => vec![format!("pacman -U --noconfirm --needed {}", path.display())],
            "apt" => vec![format!("apt-get install -y {}", path.display())],
            "zypper" => vec![format!("{} {}", self.install, path.display())],
            _ => vec![
                format!("xbps-rindex -a {}", path.display()),
                format!("xbps-install -y -R {} {names}", dir.display()),
            ],
        }
    }

    /// Answers the version queries as if `names` were installed at 3.3.
    fn answer_installed(&self, w: &World, names: &[&str]) {
        w.answer(self.version, 0, self.printed);
        if !self.batch.is_empty() {
            let sep = if self.kind == "pacman" { " " } else { "\t" };
            let out: String = names.iter().map(|n| format!("{n}{sep}3.3-1\n")).collect();
            w.answer(self.batch, 0, &out);
        }
    }

    fn is_query(&self, line: &str) -> bool {
        line.starts_with(self.version) || (!self.batch.is_empty() && line.starts_with(self.batch))
    }

    /// Leaves out version queries and the snapshot move a first Tumbleweed apply makes.
    fn without_queries(&self, lines: Vec<String>) -> Vec<String> {
        lines.into_iter().filter(|l| !self.is_query(l) && !is_snapshot_move(l)).collect()
    }
}

fn locked_file(w: &mut World, f: &Fam, version: &str, body: &[u8]) -> String {
    let file = format!("htop-{version}.x86_64.{}", f.ext);
    let url = format!("https://example/{file}");
    let artifact = Artifact { url: Some(url.clone()), hash: Some(sri(body)), ..Default::default() };
    w.lock.upsert(locked("htop", &format!("{}:", f.kind), Some(version), &[("x86_64-linux", artifact)]));
    w.fetch.bodies.insert(url, body.to_vec());
    file
}

#[test]
fn every_family_installs_by_name_reports_drift_and_removes() {
    for f in &FAMS {
        let w = f.world();
        f.answer_installed(&w, &["htop", "jq", "ripgrep"]);
        let want = [f.pkg("htop", &[]), f.pkg("tools", &["jq", "ripgrep"])];
        w.apply(Scope::System, &want).unwrap();
        let lines = w.take();
        assert_eq!(f.without_queries(lines.clone()), [format!("{} htop jq ripgrep", f.install)]);
        // One query records every version, or one per name without a batched query.
        let queries = if f.batch.is_empty() { 3 } else { 1 };
        assert_eq!(lines.iter().filter(|l| f.is_query(l)).count(), queries, "{}", f.kind);
        assert!(w.plan(Scope::System, &want).unwrap().changes.is_empty(), "{}", f.kind);

        w.forget_answers();
        f.answer_installed(&w, &["htop", "ripgrep"]);
        w.answer(&format!("{} jq", f.version), 1, "not installed");
        let plan = w.plan(Scope::System, &want).unwrap();
        assert_eq!(plan.changes.len(), 1, "{}: {plan}", f.kind);

        w.forget_answers();
        w.take();
        w.apply(Scope::System, &[]).unwrap();
        assert_eq!(w.take(), [format!("{} htop", f.remove), format!("{} jq ripgrep", f.remove)], "{}", f.kind);
    }
}

#[test]
fn every_family_detects_a_version_that_moved() {
    for f in &FAMS {
        let w = f.world();
        w.answer(f.version, 0, f.printed);
        let want = [f.pkg("htop", &[])];
        w.apply(Scope::System, &want).unwrap();
        w.forget_answers();
        let moved = f.printed.replace("3.3", "3.4");
        w.answer(f.version, 0, &moved);
        assert!(is_drift(&w.plan(Scope::System, &want).unwrap()), "{}", f.kind);
    }
}

#[test]
fn every_family_caches_the_locked_file_and_installs_it() {
    for f in &FAMS {
        let mut w = f.world();
        let file = locked_file(&mut w, f, "3.3-1", b"package one");
        let want = [f.pkg("htop", &[])];
        w.apply(Scope::System, &want).unwrap();
        let cached = w.paths.root.join("var/cache/fspec/pkgs").join(&file);
        assert_eq!(fs::read(&cached).unwrap(), b"package one", "{}", f.kind);
        assert_eq!(f.without_queries(w.take()), f.install_file(&w, &file, "htop"), "{}", f.kind);
        assert_eq!(w.state(Scope::System).entries[&want[0].resource.key()].record["file"], file.as_str());
        assert!(w.plan(Scope::System, &want).unwrap().changes.is_empty(), "{}", f.kind);
    }
}

#[test]
fn every_family_rejects_a_bad_hash_before_touching_the_system() {
    for f in &FAMS {
        let mut w = f.world();
        let file = locked_file(&mut w, f, "3.3-1", b"package one");
        w.fetch.bodies.insert(format!("https://example/{file}"), b"tampered".to_vec());
        let err = w.apply(Scope::System, &[f.pkg("htop", &[])]).unwrap_err();
        assert!(format!("{err:#}").contains("hashes to"), "{}: {err:#}", f.kind);
        assert!(w.take().is_empty(), "{}", f.kind);
    }
}

#[test]
fn every_family_journals_the_old_file_on_a_lock_bump() {
    for f in &FAMS {
        let mut w = f.world();
        let old = locked_file(&mut w, f, "3.3-1", b"package one");
        let want = [f.pkg("htop", &[]), f.pkg("zzz", &[])];
        w.apply(Scope::System, &want[..1]).unwrap();
        w.take();

        let new = locked_file(&mut w, f, "3.4-1", b"package two");
        assert!(is_drift(&w.plan(Scope::System, &want[..1]).unwrap()), "{}", f.kind);

        w.answer(&format!("{} zzz", f.install), 1, "");
        assert!(w.apply(Scope::System, &want).is_err(), "{}", f.kind);
        w.forget_answers();
        let mut expect = f.install_file(&w, &new, "htop");
        expect.push(format!("{} zzz", f.install));
        expect.extend(f.install_file(&w, &old, "htop"));
        assert_eq!(f.without_queries(w.take()), expect, "{}", f.kind);
    }
}

#[test]
fn every_family_keeps_the_earlier_install_when_a_later_one_fails() {
    for f in &FAMS {
        let w = f.world();
        w.answer(&format!("{} b", f.install), 1, "");
        w.answer(&format!("{} a b", f.install), 1, "");
        assert!(w.apply(Scope::System, &[f.pkg("a", &[]), f.pkg("b", &[])]).is_err());
        let lines = w.take();
        // The failed transaction of both falls back to one per package, so `a` lands.
        assert!(lines.contains(&format!("{} a", f.install)), "{}: {lines:#?}", f.kind);
        assert_eq!(lines.last(), Some(&format!("{} b", f.install)), "{}", f.kind);
        assert!(!lines.iter().any(|l| l.starts_with(f.remove)), "{}: {lines:#?}", f.kind);
    }
}

#[test]
fn apt_runs_noninteractive() {
    let w = World::on("debian", &[]);
    w.apply(Scope::System, &[FAMS[2].pkg("htop", &[])]).unwrap();
    let calls = w.runner.calls.lock().unwrap();
    let install = calls.iter().find(|c| c.program == "apt-get").unwrap();
    assert_eq!(install.env, [("DEBIAN_FRONTEND".to_string(), "noninteractive".to_string())]);
}

#[test]
fn distro_managers_claim_only_their_own_distro_and_never_image_hosts() {
    for (i, f) in FAMS.iter().enumerate() {
        for (j, other) in FAMS.iter().enumerate() {
            let w = other.world();
            let claimed = ALL.iter().filter(|b| b.handles(&w.cx(Scope::System), &f.pkg("htop", &[]).resource)).count();
            assert_eq!(claimed, usize::from(i == j), "{} source on {}", f.kind, other.distro);
        }
    }
    let image = Image {
        base: "quay.io/fedora/fedora-silverblue:42".into(),
        packages: None,
        kernel_args: vec![],
        distro: None,
        ..Default::default()
    };
    let w = World::new("x86_64-linux", Some(System::Image(image)));
    let want = FAMS[0].pkg("htop", &[]);
    assert!(!ALL.iter().any(|b| b.handles(&w.cx(Scope::System), &want.resource)));
}

fn distro_res(distro: &str, packages: &[&str]) -> Desired {
    let packages: Vec<_> = packages.iter().map(ToString::to_string).collect();
    Desired {
        resource: Resource::Distro(Distro {
            distro: distro.into(),
            init: fspec_core::facts::Init::Systemd,
            packages,
            kernel_args: vec![],
        }),
        owner: None,
    }
}

#[test]
fn distro_backend_installs_the_diff_of_the_base_packages() {
    for f in &FAMS {
        let w = World::on(f.distro, &["a", "b"]);
        w.apply(Scope::System, &[distro_res(f.distro, &["a", "b"])]).unwrap();
        assert_eq!(f.without_queries(w.take()).last().unwrap(), &format!("{} a b", f.install), "{}", f.kind);

        w.apply(Scope::System, &[distro_res(f.distro, &["b", "c"])]).unwrap();
        let lines = f.without_queries(w.take());
        assert_eq!(&lines[lines.len() - 2..], [format!("{} a", f.remove), format!("{} c", f.install)], "{}", f.kind);

        let rec = &w.state(Scope::System).entries[&distro_res(f.distro, &[]).resource.key()].record;
        assert_eq!(rec["names"], json!(["b", "c"]));
        assert!(w.plan(Scope::System, &[distro_res(f.distro, &["b", "c"])]).unwrap().changes.is_empty());
    }
}

#[test]
fn distro_backend_rolls_back_a_failed_install_and_reports_drift() {
    let f = &FAMS[1];
    let w = World::on(f.distro, &["a"]);
    w.answer(f.version, 0, "a 1.0-1");
    let want = [distro_res(f.distro, &["a"])];
    w.apply(Scope::System, &want).unwrap();
    w.take();

    w.answer(&format!("{} z", f.version), 1, "");
    w.answer(&format!("{} z", f.install), 1, "");
    assert!(w.apply(Scope::System, &[distro_res(f.distro, &["a", "z"])]).is_err());
    let lines = f.without_queries(w.take());
    assert_eq!(lines.last(), Some(&format!("{} z", f.install)), "a failed install has nothing to undo");

    w.forget_answers();
    w.answer(f.version, 1, "");
    assert!(is_drift(&w.plan(Scope::System, &want).unwrap()));
}

#[test]
fn btrfs_snapshot_is_taken_before_the_commit_only_on_btrfs() {
    let want = [distro_res("arch", &["linux"])];
    let w = World::on("arch", &["linux"]);
    w.answer("findmnt -no FSTYPE", 0, "ext4\n");
    w.apply(Scope::System, &want).unwrap();
    assert!(w.take().iter().all(|l| !l.starts_with("btrfs")));

    let w = World::on("arch", &["linux"]);
    w.answer("findmnt -no FSTYPE", 0, "btrfs\n");
    w.apply(Scope::System, &want).unwrap();
    let lines = w.take();
    let root = w.paths.system("/").display().to_string();
    let snap = format!("btrfs subvolume snapshot -r {root} {}", w.paths.system("/.snapshots/fspec-1").display());
    let at = lines.iter().position(|l| *l == snap).unwrap_or_else(|| panic!("{lines:?}"));
    let install = lines.iter().position(|l| l.starts_with("pacman -S")).unwrap();
    assert!(at < install, "{lines:?}");

    // Nothing to change, nothing to snapshot.
    w.apply(Scope::System, &want).unwrap();
    assert!(w.take().iter().all(|l| !l.starts_with("btrfs")));
}

#[test]
fn a_new_snapshot_leaves_only_the_newest_five() {
    let w = World::on("arch", &[]);
    w.answer("findmnt -no FSTYPE", 0, "btrfs\n");
    let dir = w.paths.system("/.snapshots");
    for g in [2, 3, 9, 10, 11, 12] {
        fs::create_dir_all(dir.join(format!("fspec-{g}"))).unwrap();
    }
    fs::create_dir_all(dir.join("187")).unwrap();
    w.apply(Scope::System, &[distro_res("arch", &["linux"])]).unwrap();
    let deletes: Vec<String> = w.take().into_iter().filter(|l| l.starts_with("btrfs subvolume delete")).collect();
    let del = |g: u32| format!("btrfs subvolume delete {}", dir.join(format!("fspec-{g}")).display());
    assert_eq!(deletes, [del(2)]);
}

fn moss_line(args: &[&str]) -> String {
    fspec_core::moss::moss(crate::family::LOADER_CONF, args.iter().copied()).to_string()
}

#[test]
fn moss_installs_by_name_and_keeps_the_earlier_install() {
    let w = World::on("aerynos", &[]);
    fs::create_dir_all(w.paths.root.join("usr")).unwrap();
    fs::write(w.paths.root.join("usr/.stateID"), "7\n").unwrap();
    let pkg = |n: &str| pkg(n, json!({"kind": "moss", "names": []}), "pinned");
    w.answer(&moss_line(&["-y", "install", "b"]), 1, "");
    w.answer(&moss_line(&["-y", "install", "a", "b"]), 1, "");
    assert!(w.apply(Scope::System, &[pkg("a"), pkg("b")]).is_err());
    let lines = w.take();
    assert!(lines.contains(&moss_line(&["-y", "install", "a"])), "{lines:#?}");
    assert_eq!(lines.last(), Some(&moss_line(&["-y", "install", "b"])));
    assert!(!lines.iter().any(|l| l.contains("remove")), "{lines:#?}");
}

#[test]
fn moss_versions_come_from_the_installed_list() {
    let w = World::on("aerynos", &[]);
    let query = "sh -c";
    w.answer(query, 0, "3.4.1-12\n");
    w.apply(Scope::System, &[pkg("htop", json!({"kind": "moss", "names": []}), "pinned")]).unwrap();
    let state = w.state(Scope::System);
    let rec = &state.entries.values().next().unwrap().record;
    assert_eq!(rec["versions"]["htop"], "3.4.1-12");
}

const ROLLING_OSS: &str = "[repo-oss]\nname=openSUSE-Tumbleweed-Oss\nenabled=1\nautorefresh=1\nbaseurl=http://download.opensuse.org/tumbleweed/repo/oss/\n";
const OPENH264: &str =
    "[repo-openh264]\nenabled=1\nautorefresh=1\nbaseurl=http://codecs.opensuse.org/openh264/openSUSE_Tumbleweed\n";

fn tumbleweed() -> World {
    let w = World::on(TW, &["htop"]);
    let dir = w.paths.root.join("etc/zypp/repos.d");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("repo-oss.repo"), ROLLING_OSS).unwrap();
    fs::write(dir.join("repo-openh264.repo"), OPENH264).unwrap();
    w
}

fn snapshot_var(w: &World) -> String {
    fs::read_to_string(w.paths.root.join("etc/zypp/vars.d/fspec_snapshot")).unwrap()
}

/// Stands in for zypper's own write of `modifyrepo --disable repo-oss`.
fn disable_rolling(w: &World) {
    fs::write(w.paths.root.join("etc/zypp/repos.d/repo-oss.repo"), ROLLING_OSS.replace("enabled=1", "enabled=0"))
        .unwrap();
}

#[test]
fn tumbleweed_points_the_repos_at_the_locked_snapshot_then_upgrades() {
    let w = tumbleweed();
    let want = [distro_res(TW, &["htop"])];
    w.apply(Scope::System, &want).unwrap();
    let lines: Vec<String> =
        w.take().into_iter().filter(|l| !l.starts_with("rpm -q") && !l.starts_with("findmnt")).collect();
    assert_eq!(
        lines,
        [
            "zypper --non-interactive --no-refresh install --dry-run --auto-agree-with-licenses htop",
            "zypper --non-interactive modifyrepo --disable repo-oss",
            "zypper --non-interactive refresh --force fspec-oss fspec-non-oss",
            "zypper --non-interactive dist-upgrade --auto-agree-with-licenses",
            "zypper --non-interactive install --auto-agree-with-licenses htop",
        ]
    );
    assert_eq!(snapshot_var(&w), "20261018\n");
    let oss = fs::read_to_string(w.paths.root.join("etc/zypp/repos.d/fspec-oss.repo")).unwrap();
    assert!(
        oss.contains("baseurl=https://download.opensuse.org/history/${fspec_snapshot}/tumbleweed/repo/oss/"),
        "{oss}"
    );
    let rec = &w.state(Scope::System).entries[&want[0].resource.key()].record;
    assert_eq!(rec["repo_snapshot"], "20261018");

    disable_rolling(&w);
    assert!(w.plan(Scope::System, &want).unwrap().changes.is_empty());

    let mut w = w;
    w.lock.zypper.insert("h".into(), LockedZypper { snapshot: "20261020".into() });
    let plan = w.plan(Scope::System, &want).unwrap();
    assert!(is_drift(&plan), "{plan}");
    assert!(plan.to_string().contains("repos are at snapshot 20261018, the lock says 20261020"), "{plan}");
    w.apply(Scope::System, &want).unwrap();
    let lines: Vec<String> = w.take().into_iter().filter(|l| l.starts_with("zypper")).collect();
    assert_eq!(
        lines,
        [
            "zypper --non-interactive refresh --force fspec-oss fspec-non-oss",
            "zypper --non-interactive dist-upgrade --auto-agree-with-licenses",
        ]
    );
    assert_eq!(snapshot_var(&w), "20261020\n");
}

#[test]
fn a_failed_upgrade_puts_the_old_snapshot_back_and_upgrades_against_it() {
    let mut w = tumbleweed();
    let want = [distro_res(TW, &[])];
    w.apply(Scope::System, &want).unwrap();
    disable_rolling(&w);
    w.take();

    w.lock.zypper.insert("h".into(), LockedZypper { snapshot: "20261020".into() });
    w.answer("zypper --non-interactive dist-upgrade", 1, "");
    assert!(w.apply(Scope::System, &want).is_err());
    assert_eq!(snapshot_var(&w), "20261018\n");
    let lines = w.take();
    assert_eq!(lines.last().unwrap(), "zypper --non-interactive dist-upgrade --auto-agree-with-licenses");
}

#[test]
fn a_first_pin_that_fails_reenables_the_rolling_repos() {
    let w = tumbleweed();
    w.answer("zypper --non-interactive dist-upgrade", 1, "");
    assert!(w.apply(Scope::System, &[distro_res(TW, &[])]).is_err());
    let lines = w.take();
    let n = lines.len();
    assert_eq!(
        lines[n - 2..],
        [
            "zypper --non-interactive modifyrepo --enable repo-oss".to_string(),
            "zypper --non-interactive dist-upgrade --auto-agree-with-licenses".to_string(),
        ]
    );
    assert!(!w.paths.root.join("etc/zypp/vars.d/fspec_snapshot").exists());
    assert!(!w.paths.root.join("etc/zypp/repos.d/fspec-oss.repo").exists());
}

#[test]
fn tumbleweed_without_a_pin_asks_for_fspec_update() {
    let mut w = tumbleweed();
    w.lock.zypper.clear();
    let err = w.apply(Scope::System, &[distro_res(TW, &[])]).unwrap_err();
    assert!(format!("{err:#}").contains("pins no Tumbleweed snapshot for host h"), "{err:#}");
}

#[test]
fn aarch64_tumbleweed_keeps_the_rolling_ports_repos() {
    let distro =
        Distro { distro: TW.into(), init: fspec_core::facts::Init::Systemd, packages: vec![], kernel_args: vec![] };
    let w = World::new("aarch64-linux", Some(System::Distro(distro)));
    w.apply(Scope::System, &[distro_res(TW, &[])]).unwrap();
    assert!(!w.take().iter().any(|l| l.contains("dist-upgrade") || l.contains("modifyrepo")));
    assert!(!w.paths.root.join("etc/zypp/vars.d/fspec_snapshot").exists());
    assert!(!w.paths.root.join("etc/zypp/repos.d/fspec-oss.repo").exists());
}

const ONEPASSWORD_FPR: &str = "3FEF9748469ADBE15DA7CA80AC2D62742012EA22";
const KEY_URL: &str = "https://downloads.1password.com/linux/keys/1password.asc";
const REPO_URL: &str = "https://downloads.1password.com/linux/rpm/stable/x86_64";

fn repo_res(fingerprint: &str) -> Desired {
    let r = json!({"kind": "repo", "name": "1password", "url": REPO_URL, "key": KEY_URL, "fingerprint": fingerprint});
    Desired { resource: serde_json::from_value(r).unwrap(), owner: None }
}

fn gpg_says(w: &World, fingerprint: &str) {
    w.answer(
        "gpg --batch --with-colons --show-keys",
        0,
        &format!("pub:-:4096:1:AC2D62742012EA22:::\nfpr:::::::::{fingerprint}:\n"),
    );
}

#[test]
fn a_repo_imports_its_pinned_key_before_zypper_adds_it_and_its_packages_install_after() {
    let mut w = tumbleweed();
    disable_rolling(&w);
    w.fetch.bodies.insert(KEY_URL.into(), b"armored key".to_vec());
    gpg_says(&w, ONEPASSWORD_FPR);
    w.answer("rpm -q gpg-pubkey-2012ea22", 1, "");
    let cli = pkg("1password-cli", json!({"kind": "zypper", "names": []}), "pinned");
    let want = [cli, repo_res(ONEPASSWORD_FPR), distro_res(TW, &[])];
    w.apply(Scope::System, &want).unwrap();
    let lines = w.take();
    let at = |prefix: &str| {
        lines.iter().position(|l| l.starts_with(prefix)).unwrap_or_else(|| panic!("{prefix}: {lines:#?}"))
    };
    let key = w.paths.root.join("var/cache/fspec/keys/1password.asc");
    assert_eq!(fs::read(&key).unwrap(), b"armored key");
    assert!(at("gpg") < at("rpm --import"));
    assert!(at("rpm --import") < at(&format!("zypper --non-interactive addrepo --refresh {REPO_URL} 1password")));
    assert!(at("zypper --non-interactive addrepo") < at("zypper --non-interactive dist-upgrade"));
    assert!(
        at("zypper --non-interactive dist-upgrade")
            < at("zypper --non-interactive install --auto-agree-with-licenses 1password-cli")
    );

    w.take();
    w.apply(Scope::System, &[want[0].clone(), want[2].clone()]).unwrap();
    assert!(!w.take().iter().any(|l| l.contains("removerepo")), "removerepo only runs for a repo zypper knows");
}

#[test]
fn a_repo_key_with_another_fingerprint_stops_the_apply_before_anything_changes() {
    let mut w = tumbleweed();
    w.fetch.bodies.insert(KEY_URL.into(), b"armored key".to_vec());
    gpg_says(&w, "0000000000000000000000000000000000000000");
    let err = w.apply(Scope::System, &[repo_res(ONEPASSWORD_FPR)]).unwrap_err();
    assert!(format!("{err:#}").contains("the spec pins 3FEF9748"), "{err:#}");
    assert!(w.take().iter().all(|l| l.starts_with("gpg")));
}

#[test]
fn a_known_repo_at_another_url_is_replaced_and_a_dropped_one_removed() {
    let mut w = tumbleweed();
    disable_rolling(&w);
    let file = w.paths.root.join("etc/zypp/repos.d/1password.repo");
    fs::write(&file, "[1password]\nenabled=1\nbaseurl=https://old.example/rpm\n").unwrap();
    w.fetch.bodies.insert(KEY_URL.into(), b"armored key".to_vec());
    gpg_says(&w, ONEPASSWORD_FPR);
    let want = [repo_res(ONEPASSWORD_FPR)];
    w.apply(Scope::System, &want).unwrap();
    let lines: Vec<String> = w.take().into_iter().filter(|l| l.contains("repo ")).collect();
    assert_eq!(
        lines,
        [
            "zypper --non-interactive removerepo 1password".to_string(),
            format!("zypper --non-interactive addrepo --refresh {REPO_URL} 1password"),
        ]
    );

    fs::write(&file, format!("[1password]\nenabled=1\nbaseurl={REPO_URL}\n")).unwrap();
    assert!(w.plan(Scope::System, &want).unwrap().changes.is_empty());
    w.apply(Scope::System, &[]).unwrap();
    assert_eq!(w.take(), ["zypper --non-interactive removerepo 1password"]);
}

#[test]
fn repo_is_for_zypper_distros_only() {
    let w = World::linux();
    let r = json!({"kind": "repo", "name": "x", "url": "https://example/x"});
    let want = [Desired { resource: serde_json::from_value(r).unwrap(), owner: None }];
    let err = w.apply(Scope::System, &want).unwrap_err();
    assert!(format!("{err:#}").contains("repo(...) is for zypper distros"), "{err:#}");
}

fn from_1password(name: &str) -> Desired {
    pkg(name, json!({"kind": "zypper", "names": [], "repo": "1password"}), "pinned")
}

#[test]
fn a_repo_whose_key_cannot_be_fetched_is_deferred_with_its_packages() {
    let w = tumbleweed();
    disable_rolling(&w);
    let want = [from_1password("1password-cli"), repo_res(ONEPASSWORD_FPR), distro_res(TW, &[])];
    w.apply(Scope::System, &want).unwrap();
    let lines = w.take();
    assert!(!lines.iter().any(|l| l.contains("addrepo") || l.contains("1password-cli")), "{lines:#?}");
    assert!(lines.iter().any(|l| l.contains("dist-upgrade")), "the rest of the apply goes on: {lines:#?}");
    let state = w.state(Scope::System);
    assert!(state.entries.keys().all(|k| k.kind == "distro"), "{:?}", state.entries.keys());
    let pending: Vec<String> =
        w.plan(Scope::System, &want).unwrap().changes.iter().map(|c| c.key.to_string()).collect();
    assert_eq!(pending, ["pkg:1password-cli", "repo:1password"]);
}

#[test]
fn a_repo_that_cannot_refresh_is_disabled_and_deferred() {
    let mut w = tumbleweed();
    disable_rolling(&w);
    w.fetch.bodies.insert(KEY_URL.into(), b"armored key".to_vec());
    gpg_says(&w, ONEPASSWORD_FPR);
    w.answer("zypper --non-interactive refresh 1password", 4, "");
    let want = [from_1password("1password-cli"), repo_res(ONEPASSWORD_FPR), distro_res(TW, &[])];
    w.apply(Scope::System, &want).unwrap();
    let lines = w.take();
    let at = |prefix: &str| {
        lines.iter().position(|l| l.starts_with(prefix)).unwrap_or_else(|| panic!("{prefix}: {lines:#?}"))
    };
    assert!(
        at("zypper --non-interactive refresh 1password")
            < at("zypper --non-interactive modifyrepo --disable 1password")
    );
    assert!(
        at("zypper --non-interactive modifyrepo --disable 1password") < at("zypper --non-interactive dist-upgrade")
    );
    assert!(!lines.iter().any(|l| l.contains("install --auto-agree-with-licenses 1password-cli")), "{lines:#?}");
}

#[test]
fn repo_on_a_package_needs_a_zypper_distro() {
    let w = World::linux();
    let err =
        w.apply(Scope::System, &[pkg("x", json!({"kind": "dnf", "names": [], "repo": "r"}), "pinned")]).unwrap_err();
    assert!(format!("{err:#}").contains("not a zypper distro"), "{err:#}");
}

#[test]
fn a_failed_apply_keeps_the_distro_packages_it_installed() {
    let f = &FAMS[1];
    let w = World::on(f.distro, &[]);
    w.answer(f.version, 0, "a 1.0-1");
    w.answer(&format!("{} zzz", f.install), 1, "");
    assert!(w.apply(Scope::System, &[distro_res(f.distro, &["a", "new"]), f.pkg("zzz", &[])]).is_err());
    let lines = f.without_queries(w.take());
    assert_eq!(lines.last(), Some(&format!("{} zzz", f.install)), "{lines:#?}");
    assert!(!lines.iter().any(|l| l.starts_with(f.remove)), "{lines:#?}");
}

#[test]
fn a_bang_name_swaps_out_a_conflicting_package_and_a_failed_apply_keeps_the_swap() {
    let w = tumbleweed();
    disable_rolling(&w);
    w.answer("rpm -q --whatprovides --qf %{VERSION}-%{RELEASE}\\n power-profiles-daemon", 0, "0.30-1.6");
    w.answer("rpm -q --whatprovides --qf %{VERSION}-%{RELEASE}\\n tuned-ppd", 1, "");
    let want = [
        distro_res(TW, &["tuned-ppd", "!power-profiles-daemon"]),
        pkg("zzz", json!({"kind": "zypper", "names": []}), "pinned"),
    ];
    w.answer("zypper --non-interactive install --auto-agree-with-licenses zzz", 1, "");
    assert!(w.apply(Scope::System, &want).is_err());
    let lines = w.take();
    let install = "zypper --non-interactive install --auto-agree-with-licenses tuned-ppd !power-profiles-daemon";
    assert!(lines.iter().any(|l| l == install), "{lines:#?}");
    assert!(
        !lines.iter().any(|l| l.contains("remove tuned-ppd") || l.ends_with("licenses power-profiles-daemon")),
        "{lines:#?}"
    );

    let err = World::on("arch", &[]).apply(Scope::System, &[distro_res("arch", &["!x"])]).unwrap_err();
    assert!(format!("{err:#}").contains("zypper syntax"), "{err:#}");
}

#[test]
fn a_zypper_install_whose_scriptlet_failed_counts_and_reinstalls_that_package() {
    let w = tumbleweed();
    disable_rolling(&w);
    let warning = "warning: %post(op-2.41.1-1.x86_64) scriptlet failed, exit status 127\n";
    w.answer("zypper --non-interactive install --auto-agree-with-licenses htop op", 107, warning);
    w.apply(Scope::System, &[distro_res(TW, &["htop", "op"])]).unwrap();
    let lines = w.take();
    assert!(
        lines
            .contains(&"zypper --non-interactive install --force --auto-agree-with-licenses op-2.41.1-1.x86_64".into()),
        "{lines:#?}"
    );
    assert!(!lines.iter().any(|l| l.contains("remove")), "{lines:#?}");
}

/// A root `fspec switch --beside` installed, with `script` as its ESP sync.
fn switched(w: &World, script: &str) {
    for (path, text) in [
        ("etc/systemd/system/fspec-esp-sync.service", ""),
        ("etc/systemd/system/fspec-esp-sync.path", fspec_switch::TUMBLEWEED_ESP_SYNC_PATH),
        ("etc/fspec/esp-sync.sh", script),
    ] {
        let path = w.paths.root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, text).unwrap();
    }
}

#[test]
fn apply_brings_an_older_esp_sync_script_up_to_this_fspec() {
    let w = tumbleweed();
    let want = [distro_res(TW, &[])];
    w.apply(Scope::System, &want).unwrap();
    switched(&w, "#!/bin/sh\n# an older fspec's\n");
    disable_rolling(&w);
    let plan = w.plan(Scope::System, &want).unwrap();
    assert!(plan.to_string().contains("/etc/fspec/esp-sync.sh is not this fspec's"), "{plan}");

    w.take();
    w.apply(Scope::System, &want).unwrap();
    let script = fs::read_to_string(w.paths.root.join("etc/fspec/esp-sync.sh")).unwrap();
    assert_eq!(script, fspec_switch::TUMBLEWEED_ESP_SYNC);
    assert!(w.take().contains(&"systemctl start fspec-esp-sync.service".to_string()));
    disable_rolling(&w);
    assert!(w.plan(Scope::System, &want).unwrap().changes.is_empty());
}

#[test]
fn limine_conf_on_the_esp_is_handed_to_the_esp_sync() {
    let w = tumbleweed();
    switched(&w, fspec_switch::TUMBLEWEED_ESP_SYNC);
    let file = |path: &str, text: &str| Desired {
        resource: serde_json::from_value(json!({"kind": "file", "path": path, "content": text})).unwrap(),
        owner: None,
    };
    let backends: Vec<&dyn Backend> = ALL.iter().copied().chain([&fspec_files::FileBackend as &dyn Backend]).collect();
    let esp = "/boot/efi/EFI/limine/limine.conf";
    apply_scope(&w.cx(Scope::System), &backends, &[distro_res(TW, &[]), file(esp, "timeout: 3\n")]).unwrap();
    disable_rolling(&w);

    let want = [distro_res(TW, &[]), file("/etc/fspec/limine.conf", "timeout: 3\n")];
    let plan = fspec_core::apply::plan_scope(&w.cx(Scope::System), &backends, &want, &w.state(Scope::System)).unwrap();
    assert!(plan.to_string().contains(&format!("> file:{esp}  managed handed over")), "{plan}");
    apply_scope(&w.cx(Scope::System), &backends, &want).unwrap();
    assert_eq!(fs::read_to_string(w.paths.root.join(&esp[1..])).unwrap(), "timeout: 3\n");
    let state = w.state(Scope::System);
    assert!(!state.entries.keys().any(|k| k.kind == "file" && k.id == esp), "{:?}", state.entries.keys());
}

fn with_args(mut d: Desired, args: &[&str]) -> Desired {
    let Resource::Distro(distro) = &mut d.resource else { unreachable!() };
    distro.kernel_args = args.iter().map(ToString::to_string).collect();
    d
}

#[test]
fn kernel_args_replace_their_own_and_rewrite_the_boot_entries() {
    let w = tumbleweed();
    let base = "root=UUID=u rootflags=subvol=@tumbleweed rw mitigations=auto";
    let cmdline = w.paths.root.join("etc/kernel/cmdline");
    fs::create_dir_all(cmdline.parent().unwrap()).unwrap();
    fs::write(&cmdline, format!("{base}\n")).unwrap();
    let want = [with_args(distro_res(TW, &[]), &["nvidia-drm.modeset=1", "mitigations=off"])];
    let err = w.apply(Scope::System, &want).unwrap_err();
    assert!(format!("{err:#}").contains("fspec switch --beside"), "{err:#}");

    switched(&w, fspec_switch::TUMBLEWEED_ESP_SYNC);
    disable_rolling(&w);
    w.take();
    w.apply(Scope::System, &want).unwrap();
    assert_eq!(
        fs::read_to_string(&cmdline).unwrap(),
        "root=UUID=u rootflags=subvol=@tumbleweed rw nvidia-drm.modeset=1 mitigations=off\n"
    );
    assert_eq!(w.take().iter().filter(|l| l.contains("fspec-esp-sync")).count(), 1);
    disable_rolling(&w);
    assert!(w.plan(Scope::System, &want).unwrap().changes.is_empty());

    let want = [with_args(distro_res(TW, &[]), &["acpi_backlight=native"])];
    w.apply(Scope::System, &want).unwrap();
    assert_eq!(
        fs::read_to_string(&cmdline).unwrap(),
        "root=UUID=u rootflags=subvol=@tumbleweed rw acpi_backlight=native\n"
    );

    fs::write(&cmdline, "root=UUID=u rw\n").unwrap();
    disable_rolling(&w);
    let plan = w.plan(Scope::System, &want).unwrap();
    assert!(plan.to_string().contains("not in /etc/kernel/cmdline: acpi_backlight=native"), "{plan}");
}

#[test]
fn a_cmdline_file_handed_to_kernel_args_stays_on_disk() {
    let w = tumbleweed();
    switched(&w, fspec_switch::TUMBLEWEED_ESP_SYNC);
    let base = "root=UUID=u rootflags=subvol=@tumbleweed rw quiet";
    let file = Desired {
        resource: serde_json::from_value(
            json!({"kind": "file", "path": "/etc/kernel/cmdline", "content": format!("{base}\n")}),
        )
        .unwrap(),
        owner: None,
    };
    let backends: Vec<&dyn Backend> = ALL.iter().copied().chain([&fspec_files::FileBackend as &dyn Backend]).collect();
    apply_scope(&w.cx(Scope::System), &backends, &[distro_res(TW, &[]), file]).unwrap();
    disable_rolling(&w);

    let want = [with_args(distro_res(TW, &[]), &["quiet", "mitigations=off"])];
    apply_scope(&w.cx(Scope::System), &backends, &want).unwrap();
    let cmdline = w.paths.root.join("etc/kernel/cmdline");
    assert_eq!(
        fs::read_to_string(&cmdline).unwrap(),
        "root=UUID=u rootflags=subvol=@tumbleweed rw quiet mitigations=off\n"
    );
    assert!(!w.state(Scope::System).entries.keys().any(|k| k.kind == "file"));
}

/// A first Tumbleweed apply of a desktop-sized spec, and the plan after it, in spawned commands.
#[test]
fn a_large_spec_queries_versions_in_batches() {
    let base: Vec<String> = (0..2900).map(|i| format!("p{i}")).collect();
    let extra: Vec<String> = (0..30).map(|i| format!("z{i}")).collect();
    let base_refs: Vec<&str> = base.iter().map(String::as_str).collect();
    let w = World::on(TW, &base_refs);
    let f = &FAMS[4];
    let mut want = vec![distro_res(TW, &base_refs)];
    want.extend(extra.iter().map(|n| f.pkg(n, &[])));
    let installed: Vec<&str> = base.iter().chain(&extra).map(String::as_str).collect();
    f.answer_installed(&w, &installed);

    w.apply(Scope::System, &want).unwrap();
    let applied = w.take();
    // Three queries for the base, one for the packages, and the 30 packages in one transaction.
    assert_eq!(applied.iter().filter(|l| f.is_query(l)).count(), 4, "{applied:#?}");
    assert_eq!(applied.iter().filter(|l| l.starts_with(f.install)).count(), 2, "{applied:#?}");

    assert!(w.plan(Scope::System, &want).unwrap().changes.is_empty());
    // Three queries for the base and one for every package together.
    let planned = w.take();
    assert_eq!(planned.len(), 4, "{planned:#?}");
}

const DRY_RUN: &str = "zypper --non-interactive --no-refresh install --dry-run --auto-agree-with-licenses";

#[test]
fn a_zypper_conflict_fails_in_prepare_before_the_snapshot_moves() {
    let w = tumbleweed();
    let problem = "Resolving package dependencies...\nProblem: 1: the to be installed rustup-1.28 conflicts with \
                   'rust-src' provided by the to be installed rust-src-1.90\n Solution 1: do not install rustup\n";
    w.answer(DRY_RUN, 4, problem);
    let want = [distro_res(TW, &["rust-src"]), FAMS[4].pkg("rustup", &[])];
    let err = format!("{:#}", w.apply(Scope::System, &want).unwrap_err());
    assert!(err.contains("rustup-1.28 conflicts with 'rust-src'"), "{err}");
    let lines = w.take();
    assert_eq!(lines.iter().filter(|l| l.starts_with(DRY_RUN)).count(), 1, "{lines:#?}");
    assert!(lines.iter().all(|l| !is_snapshot_move(l) && !l.starts_with(FAMS[4].install)), "{lines:#?}");
    assert_eq!(w.state(Scope::System).generation, 0);
}

#[test]
fn the_pkg_dry_run_takes_the_distro_adds_and_ignores_what_it_cannot_find() {
    let w = tumbleweed();
    // 104: a name no repo has yet, which is the install's to report, not the dry run's.
    w.answer(DRY_RUN, 104, "Package 'vendor-tool' not found.\n");
    let want = [distro_res(TW, &["htop"]), FAMS[4].pkg("rustup", &[])];
    w.apply(Scope::System, &want).unwrap();
    let runs: Vec<String> = w.take().into_iter().filter(|l| l.starts_with(DRY_RUN)).collect();
    assert_eq!(runs, [format!("{DRY_RUN} htop"), format!("{DRY_RUN} htop rustup")]);

    // Without a distro change, a new package goes straight to its install.
    disable_rolling(&w);
    let want = [distro_res(TW, &["htop"]), FAMS[4].pkg("rustup", &[]), FAMS[4].pkg("jq", &[])];
    w.apply(Scope::System, &want).unwrap();
    assert!(!w.take().iter().any(|l| l.starts_with(DRY_RUN)));
}

#[test]
fn brew_drift_lists_every_formula_and_cask_once() {
    let w = World::darwin();
    let want = [brew("fd", false, "pinned"), brew("jq", false, "pinned"), brew("ghostty", true, "pinned")];
    w.apply(Scope::User, &want).unwrap();
    w.take();
    w.answer("brew list --versions", 0, "fd 10.2.0\njq 1.7.1 1.8.0\n");
    w.answer("brew list --cask --versions", 0, "ghostty 1.2.0\n");
    assert!(w.plan(Scope::User, &want).unwrap().changes.is_empty());
    assert_eq!(w.take(), ["brew list --versions", "brew list --cask --versions"]);
}

#[test]
fn a_capability_two_rpms_provide_reads_the_first() {
    assert_eq!(crate::family::Family::Zypper.parse_version("pkgconfig(pam)", "1.7.1-1.1\n1.6.0-2.1\n"), "1.7.1-1.1");
}

#[test]
fn a_dropped_name_skips_the_dry_run_and_takes_its_deps_before_the_adds() {
    let w = tumbleweed();
    w.apply(Scope::System, &[distro_res(TW, &["rust-src"])]).unwrap();
    w.take();
    // The installed toolchain conflicts with rustup until rust-src goes, so a dry run would fail.
    w.answer(DRY_RUN, 4, "Problem: 1: rustup conflicts with 'rust+rustc'\n");
    w.apply(Scope::System, &[distro_res(TW, &["rustup"])]).unwrap();
    let lines = w.take();
    assert!(!lines.iter().any(|l| l.starts_with(DRY_RUN)), "{lines:#?}");
    let rm = lines.iter().position(|l| l == "zypper --non-interactive remove --clean-deps rust-src");
    let add = lines.iter().position(|l| l.starts_with(FAMS[4].install) && l.ends_with(" rustup"));
    assert!(rm.is_some() && rm < add, "{lines:#?}");
}

#[test]
fn a_dropped_name_a_declared_package_needs_stays_installed() {
    let w = tumbleweed();
    w.apply(Scope::System, &[distro_res(TW, &["gcc", "binutils", "go", "pkgconfig(x)", "liby", "libx-devel"])])
        .unwrap();
    w.take();
    let dry = "zypper --non-interactive --no-refresh remove --dry-run --clean-deps";
    w.answer("rpm -q --whatprovides --qf %{NAME}\\n pkgconfig(x)", 0, "libx-devel\n");
    w.answer(
        &format!("{dry} liby"),
        0,
        "The following 2 packages are going to be REMOVED:\n  liby libx-devel\n\n2 packages to remove.\n",
    );
    w.answer(
        &format!("{dry} libx-devel"),
        0,
        "The following package is going to be REMOVED:\n  libx-devel\n\n1 package to remove.\n",
    );
    w.answer(
        &format!("{dry} binutils"),
        0,
        "The following 3 packages are going to be REMOVED:\n  binutils gcc gcc15\n\n3 packages to remove.\n",
    );
    w.answer(
        &format!("{dry} go"),
        0,
        "The following 2 packages are going to be REMOVED:\n  go go1.25\n\n2 packages to remove.\n",
    );
    w.apply(Scope::System, &[distro_res(TW, &["gcc", "pkgconfig(x)"])]).unwrap();
    let lines = w.take();
    assert!(lines.contains(&format!("{} go", FAMS[4].remove)), "{lines:#?}");
    for kept in ["binutils", "liby", "libx-devel"] {
        assert!(!lines.iter().any(|l| l.starts_with(FAMS[4].remove) && l.contains(kept)), "{lines:#?}");
    }
    let rec = &w.state(Scope::System).entries[&distro_res(TW, &[]).resource.key()].record;
    assert_eq!(rec["names"], json!(["gcc", "pkgconfig(x)"]));
}

#[test]
fn drift_on_a_declared_distro_package_is_reinstalled() {
    let f = &FAMS[4];
    let w = tumbleweed();
    let want = [distro_res(TW, &["gcc", "rustup", "!rust-src"])];
    let gone = format!("{} rust-src", f.version);
    w.answer(&gone, 1, "");
    w.apply(Scope::System, &want).unwrap();
    disable_rolling(&w);
    w.take();
    w.answer(&format!("{} rustup", f.version), 1, "");
    assert!(is_drift(&w.plan(Scope::System, &want).unwrap()));
    w.apply(Scope::System, &want).unwrap();
    let installs: Vec<String> = w.take().into_iter().filter(|l| l.starts_with(f.install)).collect();
    // The `!name` rides along, so a conflicting rust-src goes in the same transaction.
    assert_eq!(installs, [format!("{} rustup !rust-src", f.install)]);

    w.forget_answers();
    w.answer(&gone, 1, "");
    let plan = w.plan(Scope::System, &want).unwrap();
    assert!(plan.changes.is_empty(), "{plan}");
}

#[test]
fn flatpak_installs_the_gl_extensions_for_the_loaded_nvidia_driver() {
    let mut w = World::linux();
    w.lock.upsert(locked("sober", "flatpak:org.vinegarhq.Sober", None, &[("any", commit("aaa"))]));
    let want = [flatpak("sober", "org.vinegarhq.Sober")];
    w.apply(Scope::System, &want).unwrap();
    w.take();

    let module = w.paths.system("/sys/module/nvidia");
    fs::create_dir_all(&module).unwrap();
    fs::write(module.join("version"), "595.112.07\n").unwrap();
    w.answer(INFO, 0, "aaa\n");
    w.answer("flatpak info --system org.freedesktop.Platform.GL", 1, "");
    let plan = w.plan(Scope::System, &want).unwrap();
    assert!(is_drift(&plan), "{plan}");
    w.take();
    w.apply(Scope::System, &want).unwrap();
    let lines = w.take();
    for arch in ["GL", "GL32"] {
        let install = format!("{INSTALL} org.freedesktop.Platform.{arch}.nvidia-595-112-07");
        assert!(lines.contains(&install), "{lines:?}");
    }

    w.forget_answers();
    w.answer(INFO, 0, "aaa\n");
    assert!(w.plan(Scope::System, &want).unwrap().changes.is_empty());
}
