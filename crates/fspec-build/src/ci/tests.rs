use fspec_core::resource::Host;
use serde_json::json;

use super::*;

fn host(name: &str, platform: &str, pkgs: &[(&str, &str)]) -> Host {
    let resources: Vec<_> = pkgs
        .iter()
        .map(|(n, kind)| match *kind {
            "git" => {
                json!({"kind": "pkg", "name": n, "source": {"kind": "git", "url": "https://example/x", "rev": "main"}})
            }
            _ => json!({"kind": "pkg", "name": n, "source": {"kind": "aqua", "name": n}}),
        })
        .collect();
    serde_json::from_value(json!({"name": name, "platform": platform, "resources": resources})).unwrap()
}

#[test]
fn jobs_are_the_git_packages_per_declared_platform() {
    let hosts = [
        host("g815", "x86_64-linux", &[("librepods", "git"), ("ripgrep", "aqua")]),
        host("e1504g", "x86_64-linux", &[("librepods", "git")]),
        host("macbook", "aarch64-darwin", &[("librepods", "git"), ("other", "git")]),
    ];
    assert_eq!(
        jobs(&hosts),
        [
            ("librepods".to_string(), "aarch64-darwin".to_string()),
            ("librepods".to_string(), "x86_64-linux".to_string()),
            ("other".to_string(), "aarch64-darwin".to_string()),
        ]
    );
}

#[test]
fn workflow_golden() {
    let jobs = vec![
        ("librepods".to_string(), "aarch64-darwin".to_string()),
        ("librepods".to_string(), "x86_64-linux".to_string()),
    ];
    let expected = include_str!("golden.yml").replace("{{repo}}", env!("CARGO_PKG_REPOSITORY"));
    assert_eq!(workflow(&jobs, "spec.star", "main").unwrap(), expected);
    let nested = workflow(&jobs, "examples/kyan/spec.star", "formalspec").unwrap();
    assert!(nested.contains("branches: [formalspec]"), "{nested}");
    assert!(
        nested.contains(r#"paths: ["examples/kyan/fspec.lock", "examples/kyan/patches/**", "examples/kyan/**.star"]"#),
        "{nested}"
    );
    assert!(nested.contains("fspec --spec examples/kyan/spec.star build --pkg"), "{nested}");
}

#[test]
fn unknown_platforms_and_empty_job_lists_fail() {
    assert!(workflow(&[], "spec.star", "main").is_err());
    assert!(workflow(&[("p".into(), "riscv64-linux".into())], "spec.star", "main").is_err());
}
