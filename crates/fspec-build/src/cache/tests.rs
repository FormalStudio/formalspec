use fspec_core::lock::LockedPkg;
use fspec_core::runner::{Output, RecordingRunner};
use serde_json::json;

use super::*;

fn out(stdout: &str) -> Output {
    Output { stdout: stdout.into(), ..Default::default() }
}

#[test]
fn registry_comes_from_the_origin_remote() {
    for (origin, want) in [
        ("git@github.com:FormalStudio/formalspec.git\n", "ghcr.io/formalstudio/fspec-cache"),
        ("https://github.com/FormalSnake/dotfiles\n", "ghcr.io/formalsnake/fspec-cache"),
    ] {
        let r = RecordingRunner::default();
        r.respond("git -C /spec remote get-url origin", out(origin));
        assert_eq!(registry(&r, Path::new("/spec"), None).unwrap(), want);
    }
    let r = RecordingRunner::default();
    r.respond("git -C /spec", out("https://gitlab.com/x/y\n"));
    assert!(registry(&r, Path::new("/spec"), None).is_err());
    assert_eq!(registry(&r, Path::new("/spec"), Some("ghcr.io/me/cache/")).unwrap(), "ghcr.io/me/cache");
}

#[test]
fn digests_land_in_the_lock_per_platform() {
    let hosts: Vec<Host> = serde_json::from_value(json!([
        {"name": "g815", "platform": "x86_64-linux", "resources": [
            {"kind": "pkg", "name": "librepods", "source": {"kind": "git", "url": "https://example/x", "rev": "main"}}]},
        {"name": "mac", "platform": "aarch64-darwin", "resources": [
            {"kind": "pkg", "name": "librepods", "source": {"kind": "git", "url": "https://example/x", "rev": "main"}}]},
    ]))
    .unwrap();
    let mut lock = Lock::default();
    lock.upsert(LockedPkg {
        name: "librepods".into(),
        source: "git:https://example/x@main".into(),
        version: None,
        rev: Some("abc".into()),
        patches: Some("sha256-P".into()),
        parts: Default::default(),
        artifacts: Default::default(),
    });
    let Resource::Pkg(pkg) = &hosts[0].resources[0] else { unreachable!() };
    let identity = crate::identity(pkg, "https://example/x", "abc", Some("sha256-P")).unwrap();
    let tag = crate::tag("abc", &identity, "x86_64-linux");
    let r = RecordingRunner::default();
    r.respond(
        &format!("oras manifest fetch --descriptor ghcr.io/o/fspec-cache/librepods:{tag}"),
        out(r#"{"mediaType":"x","digest":"sha256:dd","size":3}"#),
    );
    let (lines, errors) = record(&mut lock, &hosts, "ghcr.io/o/fspec-cache", &r).unwrap();
    assert_eq!(lines, ["librepods x86_64-linux sha256:dd"]);
    assert_eq!(errors.len(), 1, "the darwin tag returned no descriptor: {errors:?}");
    let locked = lock.named("librepods").next().unwrap();
    assert_eq!(locked.artifacts["x86_64-linux"].oci.as_deref(), Some("ghcr.io/o/fspec-cache/librepods@sha256:dd"));
    assert_eq!(locked.artifacts["x86_64-linux"].build, Some(crate::key(&identity)));
    assert_eq!(locked.patches.as_deref(), Some("sha256-P"));
}

#[test]
fn a_changed_recipe_is_another_build() {
    let pkg = |flags: serde_json::Value| -> Pkg {
        serde_json::from_value(json!({"name": "ananicy-cpp", "build_flags": flags,
            "source": {"kind": "git", "url": "https://example/a", "rev": "main"}}))
        .unwrap()
    };
    let (old, new) = (pkg(json!([])), pkg(json!(["-DUSE_BPF_PROC_IMPL=ON"])));
    let id = |p: &Pkg| crate::identity(p, "https://example/a", "abc", None).unwrap();
    assert_ne!(crate::tag("abc", &id(&old), "x86_64-linux"), crate::tag("abc", &id(&new), "x86_64-linux"));
    assert_ne!(crate::key(&id(&old)), crate::key(&id(&new)));
}
