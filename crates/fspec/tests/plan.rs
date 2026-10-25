use std::fs;
use std::path::Path;
use std::process::Command;

fn fspec(root: &Path, args: &[&str]) -> std::process::Output {
    let spec = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/basic/spec.star");
    Command::new(env!("CARGO_BIN_EXE_fspec"))
        .arg("--spec")
        .arg(spec)
        .arg("--root")
        .arg(root)
        .arg("--home")
        .arg(root.join("home/u"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn plan_against_seeded_state() {
    let root = tempfile::tempdir().unwrap();
    let out = fspec(root.path(), &["plan", "--host", "g815", "--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let plan: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let changes = plan["changes"].as_array().unwrap();
    assert!(changes.iter().all(|c| c["action"]["action"] == "add"));

    // fd was applied from another source, jq is no longer in the spec.
    let seeded = r#"{
      "version": 1,
      "entries": {
        "pkg:fd": { "resource": { "kind": "pkg", "name": "fd", "source": { "kind": "aqua", "name": "fd" } } },
        "pkg:jq": { "resource": { "kind": "pkg", "name": "jq", "source": { "kind": "aqua", "name": "jq" } } }
      }
    }"#;
    let state_dir = root.path().join("var/lib/fspec");
    fs::create_dir_all(&state_dir).unwrap();
    fs::write(state_dir.join("state.json"), seeded).unwrap();

    let out = fspec(root.path(), &["plan", "--host", "g815"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("~ pkg:fd  github:sharkdp/fd [source]"), "{text}");
    assert!(text.contains("- pkg:jq  aqua:jq"), "{text}");
    // The example's lock does not pin the engine, so the host keeps the fspec it runs.
    assert!(!text.contains("pkg:fspec") && !text.contains("/usr/local/bin/fspec"), "{text}");
    assert!(text.contains("10 to add, 1 to change, 1 to remove"), "{text}");
}

#[test]
fn unknown_host_fails() {
    let root = tempfile::tempdir().unwrap();
    let out = fspec(root.path(), &["plan", "--host", "nope"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no host named `nope`"));
}
