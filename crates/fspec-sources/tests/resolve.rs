use fspec_core::fetch::MapFetch;
use fspec_core::lock::Lock;
use fspec_core::resource::{Host, Pkg};
use fspec_core::runner::{Output, RecordingRunner};
use fspec_sources::update::update;
use fspec_sources::{Cx, parse_age, resolve};
use serde_json::{Value, json};

const NOW: i64 = 1_760_000_000; // 2025-10-09T08:53:20Z
const REGISTRY: &str = "https://raw.githubusercontent.com/aquaproj/aqua-registry/main/registry.yaml";
const HELLO_HEX: &str = "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
const HELLO_SRI: &str = "sha256-LPJNul+wow4m6DsqxbninhsWHlwfp0JecwQzYpOLmCQ=";

fn pkg(v: Value) -> Pkg {
    serde_json::from_value(v).unwrap()
}

fn host(name: &str, platform: &str, age: Option<&str>, pkgs: Vec<Value>) -> Host {
    let resources: Vec<Value> = pkgs.into_iter().map(|mut p| (p["kind"] = "pkg".into(), p).1).collect();
    serde_json::from_value(json!({"name": name, "platform": platform, "release_age": age, "resources": resources}))
        .unwrap()
}

fn asset(name: &str) -> Value {
    json!({"name": name, "browser_download_url": format!("https://dl/{name}"), "digest": HELLO_HEX})
}

fn release(tag: &str, published: &str, assets: &[&str]) -> Value {
    json!({"tag_name": tag, "draft": false, "prerelease": false, "published_at": published,
           "assets": assets.iter().map(|a| asset(a)).collect::<Vec<_>>()})
}

fn cx<'a>(fetch: &'a MapFetch, runner: &'a RecordingRunner) -> Cx<'a> {
    let mut cx = Cx::new(fetch, runner);
    cx.now = NOW;
    cx
}

fn platforms(p: &[&str]) -> Vec<String> {
    p.iter().map(|s| s.to_string()).collect()
}

fn registry() -> MapFetch {
    MapFetch::default().with(REGISTRY, include_str!("fixtures/aqua-registry.yaml"))
}

#[test]
fn aqua_ripgrep() {
    let rel = release(
        "15.1.0",
        "2025-10-01T00:00:00Z",
        &["ripgrep-15.1.0-x86_64-unknown-linux-musl.tar.gz", "ripgrep-15.1.0-aarch64-apple-darwin.tar.gz"],
    );
    let fetch = registry().with("https://api.github.com/repos/BurntSushi/ripgrep/releases/latest", rel.to_string());
    let runner = RecordingRunner::default();
    let p = pkg(json!({"name": "ripgrep", "source": {"kind": "aqua", "name": "ripgrep"}}));
    let locked = resolve(&p, &platforms(&["x86_64-linux", "aarch64-darwin"]), None, &cx(&fetch, &runner)).unwrap();
    assert_eq!(locked.source, "aqua:ripgrep");
    assert_eq!(locked.version.as_deref(), Some("15.1.0"));
    let linux = &locked.artifacts["x86_64-linux"];
    assert_eq!(linux.url.as_deref(), Some("https://dl/ripgrep-15.1.0-x86_64-unknown-linux-musl.tar.gz"));
    assert_eq!(linux.hash.as_deref(), Some(HELLO_SRI));
    assert_eq!(linux.bins, ["ripgrep-15.1.0-x86_64-unknown-linux-musl/rg"]);
    assert_eq!(locked.artifacts["aarch64-darwin"].bins, ["ripgrep-15.1.0-aarch64-apple-darwin/rg"]);
}

#[test]
fn aqua_gzipped_binary_is_stored_under_the_package_name() {
    let rel = release("2026-09-28", "2025-10-01T00:00:00Z", &["rust-analyzer-x86_64-unknown-linux-gnu.gz"]);
    let fetch =
        registry().with("https://api.github.com/repos/rust-lang/rust-analyzer/releases/latest", rel.to_string());
    let runner = RecordingRunner::default();
    let p = pkg(json!({"name": "rust-analyzer", "source": {"kind": "aqua", "name": "rust-lang/rust-analyzer"}}));
    let locked = resolve(&p, &platforms(&["x86_64-linux"]), None, &cx(&fetch, &runner)).unwrap();
    assert_eq!(locked.artifacts["x86_64-linux"].bins, ["rust-analyzer"]);
}

#[test]
fn aqua_gh_strips_v_and_reports_missing_asset() {
    let rel =
        release("v2.65.0", "2025-10-01T00:00:00Z", &["gh_2.65.0_macOS_arm64.zip", "gh_2.65.0_linux_amd64.tar.gz"]);
    let fetch = registry().with("https://api.github.com/repos/cli/cli/releases/latest", rel.to_string());
    let runner = RecordingRunner::default();
    let p = pkg(json!({"name": "gh", "source": {"kind": "aqua", "name": "github/gh"}}));
    let locked = resolve(&p, &platforms(&["aarch64-darwin", "x86_64-linux"]), None, &cx(&fetch, &runner)).unwrap();
    assert_eq!(locked.version.as_deref(), Some("2.65.0"));
    assert_eq!(locked.artifacts["aarch64-darwin"].bins, ["gh_2.65.0_macOS_arm64/bin/gh"]);
    let err = resolve(&p, &platforms(&["aarch64-linux"]), None, &cx(&fetch, &runner)).unwrap_err().to_string();
    assert!(err.contains("gh_2.65.0_linux_arm64.tar.gz") && err.contains("gh_2.65.0_macOS_arm64.zip"), "{err}");
}

#[test]
fn github_matches_per_platform() {
    let rel = release(
        "v10.2.0",
        "2025-10-01T00:00:00Z",
        &[
            "fd-v10.2.0-x86_64-unknown-linux-gnu.tar.gz",
            "fd-v10.2.0-x86_64-unknown-linux-musl.tar.gz",
            "fd-v10.2.0-aarch64-apple-darwin.tar.gz",
            "fd_10.2.0_amd64.deb",
        ],
    );
    let fetch = MapFetch::default().with("https://api.github.com/repos/sharkdp/fd/releases/latest", rel.to_string());
    let runner = RecordingRunner::default();
    let p = pkg(json!({"name": "fd", "source": {"kind": "github", "repo": "sharkdp/fd"}}));
    let locked = resolve(&p, &platforms(&["x86_64-linux", "aarch64-darwin"]), None, &cx(&fetch, &runner)).unwrap();
    assert_eq!(locked.source, "github:sharkdp/fd");
    assert_eq!(locked.version.as_deref(), Some("10.2.0"));
    assert!(locked.artifacts["x86_64-linux"].url.as_deref().unwrap().ends_with("linux-musl.tar.gz"));
    assert!(locked.artifacts["aarch64-darwin"].url.as_deref().unwrap().ends_with("aarch64-apple-darwin.tar.gz"));
}

#[test]
fn npm_records_tarball_and_integrity() {
    let doc = json!({
        "dist-tags": {"latest": "2.0.1"},
        "versions": {
            "2.0.0": {"dist": {"tarball": "https://r/c-2.0.0.tgz", "integrity": "sha512-old"}},
            "2.0.1": {"dist": {"tarball": "https://r/c-2.0.1.tgz", "integrity": "sha512-new"}}
        },
        "time": {"2.0.0": "2025-09-01T00:00:00.000Z", "2.0.1": "2025-10-08T00:00:00.000Z"}
    });
    let fetch = MapFetch::default().with("https://registry.npmjs.org/@anthropic-ai/claude-code", doc.to_string());
    let runner = RecordingRunner::default();
    let p = pkg(json!({"name": "claude-code", "source": {"kind": "npm", "name": "@anthropic-ai/claude-code"}}));
    let locked = resolve(&p, &platforms(&["x86_64-linux"]), None, &cx(&fetch, &runner)).unwrap();
    assert_eq!(locked.version.as_deref(), Some("2.0.1"));
    let any = &locked.artifacts["any"];
    assert_eq!((any.url.as_deref(), any.hash.as_deref()), (Some("https://r/c-2.0.1.tgz"), Some("sha512-new")));
    assert_eq!(locked.artifacts.len(), 1);
    // Two days old at NOW: a 3d policy falls back to 2.0.0.
    let aged =
        resolve(&p, &platforms(&["x86_64-linux"]), Some(parse_age("3d").unwrap()), &cx(&fetch, &runner)).unwrap();
    assert_eq!(aged.version.as_deref(), Some("2.0.0"));
}

fn crates_io(repo: &str) -> String {
    json!({"crate": {"max_version": "1.4.0", "repository": repo},
           "versions": [{"num": "1.4.0", "created_at": "2025-10-01T00:00:00+00:00", "yanked": false}]})
    .to_string()
}

#[test]
fn cargo_uses_the_tagged_release_or_errors() {
    let rel = release("v1.4.0", "2025-10-01T00:00:00Z", &["tool-1.4.0-x86_64-unknown-linux-musl.tar.gz"]);
    let fetch = MapFetch::default()
        .with("https://crates.io/api/v1/crates/tool", crates_io("https://github.com/o/tool"))
        .with("https://api.github.com/repos/o/tool/releases/tags/v1.4.0", rel.to_string());
    let runner = RecordingRunner::default();
    let p = pkg(json!({"name": "tool", "source": {"kind": "cargo", "name": "tool"}}));
    let locked = resolve(&p, &platforms(&["x86_64-linux"]), None, &cx(&fetch, &runner)).unwrap();
    assert_eq!(locked.version.as_deref(), Some("1.4.0"));
    assert!(locked.artifacts["x86_64-linux"].url.is_some());

    let err = resolve(&p, &platforms(&["x86_64-linux", "aarch64-darwin"]), None, &cx(&fetch, &runner)).unwrap_err();
    assert!(err.to_string().contains("no prebuilt for aarch64-darwin"), "{err:#}");

    let fetch =
        MapFetch::default().with("https://crates.io/api/v1/crates/tool", crates_io("https://gitlab.com/o/tool"));
    let err = resolve(&p, &platforms(&["x86_64-linux"]), None, &cx(&fetch, &runner)).unwrap_err();
    assert!(err.to_string().contains("no prebuilt for x86_64-linux"), "{err:#}");
}

fn aged_fetch() -> MapFetch {
    let list = json!([
        release("v3.0.0", "2025-10-08T00:00:00Z", &["t-3.0.0-linux-x86_64.tar.gz"]),
        release("v2.0.0", "2025-10-01T00:00:00Z", &["t-2.0.0-linux-x86_64.tar.gz"]),
        release("v1.0.0", "2025-09-01T00:00:00Z", &["t-1.0.0-linux-x86_64.tar.gz"]),
    ]);
    MapFetch::default().with("https://api.github.com/repos/o/t/releases?per_page=100", list.to_string()).with(
        "https://api.github.com/repos/o/t/releases/latest",
        release("v3.0.0", "2025-10-08T00:00:00Z", &["t-3.0.0-linux-x86_64.tar.gz"]).to_string(),
    )
}

#[test]
fn release_age_picks_the_newest_old_enough_and_bleeding_skips_it() {
    let (fetch, runner) = (aged_fetch(), RecordingRunner::default());
    let age = Some(parse_age("3d").unwrap());
    let stable = pkg(json!({"name": "t", "source": {"kind": "github", "repo": "o/t"}}));
    let got = resolve(&stable, &platforms(&["x86_64-linux"]), age, &cx(&fetch, &runner)).unwrap();
    assert_eq!(got.version.as_deref(), Some("2.0.0"));
    let got =
        resolve(&stable, &platforms(&["x86_64-linux"]), Some(parse_age("30d").unwrap()), &cx(&fetch, &runner)).unwrap();
    assert_eq!(got.version.as_deref(), Some("1.0.0"));
    let err = resolve(&stable, &platforms(&["x86_64-linux"]), Some(parse_age("90d").unwrap()), &cx(&fetch, &runner));
    assert!(err.unwrap_err().to_string().contains("old enough"));

    let bleeding = pkg(json!({"name": "t", "channel": "bleeding", "source": {"kind": "github", "repo": "o/t"}}));
    let got = resolve(&bleeding, &platforms(&["x86_64-linux"]), age, &cx(&fetch, &runner)).unwrap();
    assert_eq!(got.version.as_deref(), Some("3.0.0"));
}

#[test]
fn runner_backed_sources() {
    let (fetch, runner) = (MapFetch::default(), RecordingRunner::default());
    runner.respond(
        "flatpak remote-info --system kde org.kde.kate",
        Output {
            stdout: "Ref: app/org.kde.kate/x86_64/stable\nVersion: 24.12\nCommit: abc123\n".into(),
            ..Default::default()
        },
    );
    runner.respond(
        "git ls-remote https://h/r main",
        Output { stdout: "deadbeef\trefs/heads/main\n".into(), ..Default::default() },
    );
    runner.respond("nix eval --raw nixpkgs#hello.version", Output { stdout: "2.12.1\n".into(), ..Default::default() });
    let cx = cx(&fetch, &runner);
    let kate = resolve(
        &pkg(json!({"name": "kate", "source": {"kind": "flatpak", "id": "org.kde.kate", "remote": "kde"}})),
        &[],
        None,
        &cx,
    )
    .unwrap();
    assert_eq!(kate.artifacts["any"].commit.as_deref(), Some("abc123"));
    assert_eq!(kate.version.as_deref(), Some("24.12"));
    let git =
        resolve(&pkg(json!({"name": "r", "source": {"kind": "git", "url": "https://h/r"}})), &[], None, &cx).unwrap();
    assert_eq!(git.rev.as_deref(), Some("deadbeef"));
    let nix =
        resolve(&pkg(json!({"name": "hello", "source": {"kind": "nix", "attr": "hello"}})), &[], None, &cx).unwrap();
    assert_eq!(nix.version.as_deref(), Some("2.12.1"));
    let dnf =
        resolve(&pkg(json!({"name": "htop", "source": {"kind": "dnf", "names": ["htop"]}})), &[], None, &cx).unwrap();
    assert_eq!((dnf.source.as_str(), dnf.version), ("dnf:htop", None));
}

#[test]
fn flathub_resolves_over_http_without_flatpak() {
    let ff = "a".repeat(64);
    let layer = "b".repeat(64);
    let fetch = MapFetch::default()
        .with("https://dl.flathub.org/repo/refs/heads/app/org.mozilla.firefox/aarch64/stable", format!("{ff}\n"))
        .with("https://flathub.org/api/v2/appstream/org.mozilla.firefox", r#"{"releases": [{"version": "131.0"}]}"#)
        .with(
            "https://dl.flathub.org/repo/refs/heads/runtime/org.freedesktop.Platform.VulkanLayer.MangoHud/x86_64/25.08",
            layer.clone(),
        );
    let runner = RecordingRunner::default();
    let cx = cx(&fetch, &runner);
    let got = resolve(
        &pkg(json!({"name": "firefox", "source": {"kind": "flatpak", "id": "org.mozilla.firefox"}})),
        &platforms(&["aarch64-darwin", "aarch64-linux"]),
        None,
        &cx,
    )
    .unwrap();
    assert_eq!((got.artifacts["any"].commit.as_deref(), got.version.as_deref()), (Some(ff.as_str()), Some("131.0")));
    let id = "org.freedesktop.Platform.VulkanLayer.MangoHud/x86_64/25.08";
    let got = resolve(&pkg(json!({"name": "m", "source": {"kind": "flatpak", "id": id}})), &[], None, &cx).unwrap();
    assert_eq!((got.artifacts["any"].commit.as_deref(), got.version), (Some(layer.as_str()), None));
    assert!(runner.lines().is_empty(), "{:?}", runner.lines());
}

#[test]
fn brew_formula_and_cask() {
    let fetch = MapFetch::default()
        .with("https://formulae.brew.sh/api/formula/jq.json", r#"{"versions": {"stable": "1.7.1"}}"#)
        .with("https://formulae.brew.sh/api/cask/aerospace.json", r#"{"version": "0.15.2"}"#);
    let runner = RecordingRunner::default();
    let cx = cx(&fetch, &runner);
    let jq = resolve(&pkg(json!({"name": "jq", "source": {"kind": "brew"}})), &[], None, &cx).unwrap();
    assert_eq!(jq.version.as_deref(), Some("1.7.1"));
    let cask =
        resolve(&pkg(json!({"name": "aerospace", "source": {"kind": "brew", "cask": true}})), &[], None, &cx).unwrap();
    assert_eq!(cask.version.as_deref(), Some("0.15.2"));
}

#[test]
fn update_keeps_unnamed_drops_undeclared_and_reports_changes() {
    let fetch = aged_fetch().with(
        "https://api.github.com/repos/o/u/releases/latest",
        release("v5.0.0", "2025-10-01T00:00:00Z", &["u-5.0.0-linux-x86_64.tar.gz"]).to_string(),
    );
    let runner = RecordingRunner::default();
    let t = json!({"name": "t", "source": {"kind": "github", "repo": "o/t"}});
    let u = json!({"name": "u", "source": {"kind": "github", "repo": "o/u"}});
    let hosts = vec![host("a", "x86_64-linux", Some("3d"), vec![t.clone(), u.clone()])];

    let old: Lock = toml_lock(
        r#"
[[pkg]]
name = "t"
source = "github:o/t"
version = "1.0.0"

[[pkg]]
name = "u"
source = "github:o/u"
version = "4.0.0"

[[pkg]]
name = "gone"
source = "npm:gone"
version = "0.1.0"
"#,
    );
    let got = update(&hosts, &old, &["t".into()], &cx(&fetch, &runner)).unwrap();
    assert!(got.errors.is_empty(), "{:?}", got.errors);
    assert_eq!(got.changes, ["- gone 0.1.0", "t 1.0.0 -> 2.0.0"]);
    let names: Vec<_> = got.lock.pkg.iter().map(|p| (p.name.as_str(), p.version.as_deref().unwrap())).collect();
    assert_eq!(names, [("t", "2.0.0"), ("u", "4.0.0")]);

    // The delay applies per package: u is only declared on a host without one.
    let hosts = vec![
        host("a", "x86_64-linux", Some("3d"), vec![t.clone()]),
        host("b", "x86_64-linux", None, vec![t.clone(), u.clone()]),
    ];
    let got = update(&hosts, &Lock::default(), &[], &cx(&fetch, &runner)).unwrap();
    assert_eq!(got.changes, ["+ t 2.0.0", "+ u 5.0.0"]);
    assert!(update(&hosts, &Lock::default(), &["nope".into()], &cx(&fetch, &runner)).is_err());
}

#[test]
fn update_collects_platforms_across_hosts_and_survives_a_failure() {
    let rel = release("v1.0.0", "2025-09-01T00:00:00Z", &["t-linux-x86_64.tar.gz", "t-macos-arm64.tar.gz"]);
    let fetch = MapFetch::default().with("https://api.github.com/repos/o/t/releases/latest", rel.to_string());
    let runner = RecordingRunner::default();
    let t = json!({"name": "t", "source": {"kind": "github", "repo": "o/t"}});
    let bad = json!({"name": "bad", "source": {"kind": "npm", "name": "bad"}});
    let hosts = vec![
        host("a", "x86_64-linux", None, vec![t.clone(), bad.clone()]),
        host("b", "aarch64-darwin", None, vec![t.clone()]),
    ];
    let got = update(&hosts, &Lock::default(), &[], &cx(&fetch, &runner)).unwrap();
    assert_eq!(
        got.lock.named("t").next().unwrap().artifacts.keys().collect::<Vec<_>>(),
        ["aarch64-darwin", "x86_64-linux"]
    );
    assert_eq!(got.errors.len(), 1);
    assert!(got.errors[0].contains("bad"), "{:?}", got.errors);
}

#[test]
fn hosts_with_their_own_asset_glob_each_lock_their_own_asset() {
    let rel = release("v0.1.3", "2025-09-01T00:00:00Z", &["t3-0.1.3-linux-x64.tar.gz", "t3-0.1.3-darwin-arm64.tar.gz"]);
    let fetch = MapFetch::default().with("https://api.github.com/repos/o/t3/releases/tags/v0.1.3", rel.to_string());
    let runner = RecordingRunner::default();
    let t3 = |glob: &str, version: &str| json!({"name": "t3", "version": version, "source": {"kind": "github", "repo": "o/t3", "asset": glob}});
    let hosts = vec![
        host("linux", "x86_64-linux", None, vec![t3("t3-*-linux-x64.tar.gz", "0.1.3")]),
        host("mac", "aarch64-darwin", None, vec![t3("t3-*-darwin-arm64.tar.gz", "0.1.3")]),
    ];
    let got = update(&hosts, &Lock::default(), &[], &cx(&fetch, &runner)).unwrap();
    assert!(got.errors.is_empty(), "{:?}", got.errors);
    let t = got.lock.named("t3").next().unwrap();
    assert!(t.artifacts["x86_64-linux"].url.as_deref().unwrap().ends_with("linux-x64.tar.gz"));
    assert!(t.artifacts["aarch64-darwin"].url.as_deref().unwrap().ends_with("darwin-arm64.tar.gz"));

    let rel2 = release("v0.1.2", "2025-08-01T00:00:00Z", &["t3-0.1.2-darwin-arm64.tar.gz"]);
    let fetch = fetch.with("https://api.github.com/repos/o/t3/releases/tags/v0.1.2", rel2.to_string());
    let hosts = vec![
        host("linux", "x86_64-linux", None, vec![t3("t3-*-linux-x64.tar.gz", "0.1.3")]),
        host("mac", "aarch64-darwin", None, vec![t3("t3-*-darwin-arm64.tar.gz", "0.1.2")]),
    ];
    let got = update(&hosts, &Lock::default(), &[], &cx(&fetch, &runner)).unwrap();
    assert!(got.errors.iter().any(|e| e.contains("declare one version")), "{:?}", got.errors);
}

fn toml_lock(text: &str) -> Lock {
    toml::from_str(text).unwrap()
}

const FULL_SHA: &str = "4f1c2a9d3e5b7a8c9d0e1f2a3b4c5d6e7f8a9b0c";

fn patched_git(patches: &[&str]) -> Pkg {
    pkg(json!({
        "name": "tool",
        "source": {"kind": "git", "url": "https://example/tool", "rev": FULL_SHA},
        "patches": patches,
    }))
}

#[test]
fn a_patched_package_locks_the_hash_of_its_patch_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("patches")).unwrap();
    std::fs::write(dir.path().join("patches/a.patch"), "one").unwrap();
    let (fetch, runner) = (MapFetch::default(), RecordingRunner::default());
    let cx = Cx::new(&fetch, &runner).with_spec_dir(dir.path());
    let locked = resolve(&patched_git(&["patches/a.patch"]), &platforms(&["x86_64-linux"]), None, &cx).unwrap();
    assert_eq!(locked.rev.as_deref(), Some(FULL_SHA));
    assert_eq!(locked.patches, Some(fspec_core::patches::hash(dir.path(), &["patches/a.patch".to_string()]).unwrap()));
    assert_eq!(resolve(&patched_git(&[]), &platforms(&["x86_64-linux"]), None, &cx).unwrap().patches, None);
}

#[test]
fn update_keeps_recorded_oci_digests_while_commit_and_patches_stand() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("patches")).unwrap();
    std::fs::write(dir.path().join("patches/a.patch"), "one").unwrap();
    let hosts =
        [host("m", "x86_64-linux", None, vec![serde_json::to_value(patched_git(&["patches/a.patch"])).unwrap()])];
    let (fetch, runner) = (MapFetch::default(), RecordingRunner::default());
    let cx = Cx::new(&fetch, &runner).with_spec_dir(dir.path());

    let mut old = update(&hosts, &Lock::default(), &[], &cx).unwrap().lock;
    let mut locked = old.named("tool").next().unwrap().clone();
    locked.artifacts.entry("x86_64-linux".into()).or_default().oci =
        Some("ghcr.io/o/fspec-cache/tool@sha256:dd".into());
    old.upsert(locked);

    let again = update(&hosts, &old, &[], &cx).unwrap();
    assert_eq!(again.lock, old);
    assert!(again.changes.is_empty(), "{:?}", again.changes);

    std::fs::write(dir.path().join("patches/a.patch"), "two").unwrap();
    let changed = update(&hosts, &old, &[], &cx).unwrap();
    assert!(changed.lock.named("tool").next().unwrap().artifacts.is_empty());
}

#[test]
fn update_pins_tumbleweed_hosts_to_the_newest_snapshot_old_enough() {
    let fetch =
        MapFetch::default().with("https://download.opensuse.org/history/list", "20251008\n20251005\n20251001\n");
    let runner = RecordingRunner::default();
    let tw = |name: &str, age: Option<&str>| {
        let mut h = host(name, "x86_64-linux", age, vec![]);
        h.system = Some(
            serde_json::from_value(json!({"kind": "distro", "distro": "opensuse-tumbleweed", "init": "systemd"}))
                .unwrap(),
        );
        h
    };
    let hosts = vec![tw("laptop", None), tw("server", Some("7d")), host("mac", "aarch64-darwin", None, vec![])];
    let old = toml_lock("[zypper.laptop]\nsnapshot = \"20251001\"\n\n[zypper.gone]\nsnapshot = \"20251001\"\n");
    let got = update(&hosts, &old, &[], &cx(&fetch, &runner)).unwrap();
    assert!(got.errors.is_empty(), "{:?}", got.errors);
    assert_eq!(got.lock.zypper["laptop"].snapshot, "20251008");
    assert_eq!(got.lock.zypper["server"].snapshot, "20251001");
    assert!(!got.lock.zypper.contains_key("gone"));
    assert_eq!(
        got.changes,
        ["snapshot laptop 20251001 -> 20251008", "+ snapshot server 20251001", "- snapshot gone 20251001"]
    );
    let text = toml::to_string(&got.lock).unwrap();
    assert!(text.contains("[zypper.laptop]\nsnapshot = \"20251008\""), "{text}");
}

/// GitHub's API out of requests for the hour.
struct RateLimited;

impl fspec_core::fetch::Fetch for RateLimited {
    fn get(&self, url: &str) -> anyhow::Result<Vec<u8>> {
        let note = Some("resets in 12 min".to_string());
        Err(fspec_core::fetch::HttpError { url: url.into(), status: 403, rate_limited: note }.into())
    }
}

#[test]
fn a_rate_limit_is_reported_as_one_not_as_a_missing_release() {
    let runner = RecordingRunner::default();
    let mut limited = Cx::new(&RateLimited, &runner);
    limited.now = NOW;
    let p = pkg(json!({"name": "fd", "version": "10.2.0", "source": {"kind": "github", "repo": "sharkdp/fd"}}));
    let err = format!("{:#}", resolve(&p, &platforms(&["x86_64-linux"]), None, &limited).unwrap_err());
    assert!(err.contains("GitHub API rate limit reached (resets in 12 min)"), "{err}");
    assert!(!err.contains("has no release"), "{err}");

    let fetch = MapFetch::default().with(
        "https://api.github.com/repos/sharkdp/fd/releases/tags/10.2.0",
        release("10.2.0", "2025-10-01T00:00:00Z", &["fd-10.2.0-x86_64-unknown-linux-musl.tar.gz"]).to_string(),
    );
    let locked = resolve(&p, &platforms(&["x86_64-linux"]), None, &cx(&fetch, &runner)).unwrap();
    assert_eq!(locked.version.as_deref(), Some("10.2.0"));
}

#[test]
fn parts_lock_their_url_and_hash() {
    let fetch = MapFetch::default().with("https://h/main.tar.br", "main").with("https://h/core.tar.br", "core");
    let runner = RecordingRunner::default();
    let cx = cx(&fetch, &runner);
    let p = pkg(json!({"name": "discord", "source": {"kind": "url", "url": "https://h/main.tar.br"},
        "parts": {"modules/core": {"kind": "url", "url": "https://h/core.tar.br"}}}));
    let locked = resolve(&p, &[], None, &cx).unwrap();
    let part = &locked.parts["modules/core"];
    assert_eq!(part.url.as_deref(), Some("https://h/core.tar.br"));
    assert_eq!(part.hash.as_deref(), Some("sha256-DUX1/UYrjHC/+xACGsG8/z9Y8psfr3VoWVCVQn1CgSw="));
}

#[test]
fn a_git_branch_with_paths_locks_its_newest_commit_touching_them() {
    let tip = "a".repeat(40);
    let touched = "b".repeat(40);
    let runner = RecordingRunner::default();
    runner.respond(
        "git ls-remote https://x/y main",
        Output { status: 0, stdout: format!("{tip}\trefs/heads/main\n"), stderr: String::new() },
    );
    runner.respond("git -C ", Output { status: 0, stdout: format!("{touched}\n"), stderr: String::new() });
    let fetch = MapFetch::default();
    let p = pkg(
        json!({"name": "tool", "source": {"kind": "git", "url": "https://x/y", "rev": "main", "paths": ["crates", "Cargo.lock"]}}),
    );
    let locked = resolve(&p, &platforms(&["x86_64-linux"]), None, &cx(&fetch, &runner)).unwrap();
    assert_eq!(locked.rev.as_deref(), Some(touched.as_str()));
    let lines = runner.lines();
    assert!(
        lines[1].starts_with("git clone -q --bare --filter=blob:none --single-branch --branch main https://x/y "),
        "{lines:?}"
    );
    assert!(lines[2].ends_with(&format!("log -1 --format=%H {tip} -- crates Cargo.lock")), "{lines:?}");
}

#[test]
fn a_git_tag_ignores_paths() {
    let tag = "c".repeat(40);
    let runner = RecordingRunner::default();
    runner.respond(
        "git ls-remote",
        Output { status: 0, stdout: format!("{tag}\trefs/tags/v1\n"), stderr: String::new() },
    );
    let fetch = MapFetch::default();
    let p =
        pkg(json!({"name": "tool", "source": {"kind": "git", "url": "https://x/y", "rev": "v1", "paths": ["crates"]}}));
    let locked = resolve(&p, &platforms(&["x86_64-linux"]), None, &cx(&fetch, &runner)).unwrap();
    assert_eq!(locked.rev.as_deref(), Some(tag.as_str()));
    assert_eq!(runner.lines().len(), 1);
}

fn searching<'a>(fetch: &'a MapFetch, runner: &'a RecordingRunner, cache: &std::path::Path) -> Cx<'a> {
    let net =
        MapFetch::default().with("https://flathub.org/api/v2/appstream", r#"["org.mozilla.firefox","org.gnome.Maps"]"#);
    let mut cx = cx(fetch, runner).with_catalog(fspec_sources::catalog::Catalog {
        cache: cache.to_path_buf(),
        brew: None,
        net: Some(Box::new(net)),
        appstream: vec![],
    });
    cx.local = ("x86_64-linux".into(), "opensuse-tumbleweed".into());
    cx
}

#[test]
fn an_unknown_name_lists_close_spellings_and_where_else_it_is() {
    let dir = tempfile::tempdir().unwrap();
    let fetch = registry();
    let runner = RecordingRunner::default();
    runner.respond(
        "zypper",
        Output {
            status: 0,
            stdout: r#"<solvable status="not-installed" name="MozillaFirefox" summary="Mozilla Firefox Web Browser" kind="package"/>"#
                .into(),
            stderr: String::new(),
        },
    );
    let cx = searching(&fetch, &runner, dir.path());
    let err = |name: &str| {
        let p = pkg(json!({"name": name, "source": {"kind": "aqua", "name": name}}));
        format!("{:#}", resolve(&p, &platforms(&["x86_64-linux"]), None, &cx).unwrap_err())
    };
    let typo = err("rpigrep");
    assert!(typo.contains("`rpigrep` is not in the aqua registry; similar packages:"), "{typo}");
    assert!(typo.contains("fspec run ripgrep"), "{typo}");
    let app = err("firefox");
    let rows: Vec<&str> = app.lines().skip(1).collect();
    assert!(
        rows[0].contains("flatpak:org.mozilla.firefox") && rows[0].contains("fspec run flatpak:org.mozilla.firefox"),
        "{app}"
    );
    assert!(
        rows[1].contains("zypper:MozillaFirefox")
            && rows[1].contains("Mozilla Firefox Web Browser")
            && rows[1].contains("install only: pkg(\"MozillaFirefox\", source = zypper())"),
        "{app}"
    );
    assert_eq!(runner.lines()[0], "zypper --no-refresh -x -q search -t package -- rpigrep");
    // The Flathub ids are kept for a day rather than fetched per name.
    assert!(dir.path().join("flathub-ids.json").is_file());
}

#[test]
fn without_a_catalog_an_unknown_name_only_says_so() {
    let fetch = registry();
    let runner = RecordingRunner::default();
    let p = pkg(json!({"name": "firefox", "source": {"kind": "aqua", "name": "firefox"}}));
    let e = format!("{:#}", resolve(&p, &platforms(&["x86_64-linux"]), None, &cx(&fetch, &runner)).unwrap_err());
    assert_eq!(e, "resolving firefox (aqua:firefox): `firefox` is not in the aqua registry");
    assert!(runner.lines().is_empty());
}
