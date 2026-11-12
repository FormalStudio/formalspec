//! The fspec every Linux host runs. It is a `git(...)` package the evaluator adds to each Linux
//! host itself, so `fspec update` locks it, CI builds it and `fspec apply` installs it like any
//! other package, and no spec can leave it out.

use crate::lock::Lock;
use crate::resource::{Desired, Pkg, Resource, Source};

pub const NAME: &str = "fspec";
pub const REPO: &str = "https://github.com/FormalStudio/formalspec";
/// Where `fspec switch` and `fspec disk init` put fspec, and what timers and remote applies run.
pub const BIN: &str = "/usr/local/bin/fspec";
const LINK: &str = "/opt/fspec/current/bin/fspec";

// Static musl, so one binary runs on every distro and inside a root being installed. The same
// image installer/iso/build.sh builds with.
const IMAGE: &str =
    "docker.io/library/rust:1.91-alpine3.22@sha256:8efbfb788786eeb127adc581394349c5fb567712156e0f8c2e499acadbc23756";
const BUILD: &str = r#"cd "$FSPEC_SRC" && t="$(uname -m)-unknown-linux-musl" && cargo build --release --locked --target "$t" -p fspec && install -Dm755 "target/$t/release/fspec" "$FSPEC_OUT/bin/fspec""#;
// macOS has no static target and no container to build in, so a Mac compiles against its own
// libSystem with the user's rustup toolchain. BSD install has no -D.
const BUILD_DARWIN: &str = r#"cd "$FSPEC_SRC" && PATH="$HOME/.cargo/bin:$PATH" cargo build --release --locked -p fspec && mkdir -p "$FSPEC_OUT/bin" && install -m 755 target/release/fspec "$FSPEC_OUT/bin/fspec""#;

pub fn pkg() -> Pkg {
    serde_json::from_value(serde_json::json!({
        "name": NAME,
        "source": {
            "kind": "git",
            "url": REPO,
            "rev": "main",
            // The lock in examples/ lives on the same main, so the tip moves on every lock commit.
            "paths": ["crates", "Cargo.toml", "Cargo.lock"],
            "image": IMAGE,
            "deps": ["build-base"],
        },
        "build": ["sh", "-c", BUILD],
        "bins": [NAME],
        "files": [{"path": BIN, "mode": "link", "target": LINK}],
    }))
    .expect("the engine package is a valid pkg")
}

/// [`pkg`] as a remote engine upgrade builds it for `platform`. macOS hosts leave the engine
/// package out of their spec, but an older fspec on one still needs the locked engine.
pub fn build_for(platform: &str) -> Pkg {
    let mut p = pkg();
    if platform.ends_with("-darwin") {
        if let Source::Git { image, deps, .. } = &mut p.source {
            *image = None;
            deps.clear();
        }
        p.build = vec!["sh".into(), "-c".into(), BUILD_DARWIN.into()];
    }
    p
}

/// `desired` without the engine and its link while `lock` does not pin the engine: a spec repo
/// the installer just wrote, or one from before the engine was a package. The host keeps the
/// fspec it runs until `fspec update` locks one.
pub fn unless_unlocked(desired: Vec<Desired>, lock: &Lock) -> Vec<Desired> {
    let source = pkg().source;
    if lock.pkg(NAME, &source.to_string()).is_some() {
        return desired;
    }
    let implicit = |d: &Desired| match &d.resource {
        Resource::Pkg(p) => p.name == NAME && p.source == source,
        Resource::File(f) => f.path == BIN && d.owner.as_deref() == Some(NAME),
        _ => false,
    };
    desired.into_iter().filter(|d| !implicit(d)).collect()
}

/// The engine package or its link, which a test applying only OS backends leaves out.
pub fn is_engine(r: &Resource) -> bool {
    match r {
        Resource::Pkg(p) => p.name == NAME,
        Resource::File(f) => f.path == BIN,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mac_builds_the_engine_natively() {
        let p = build_for("aarch64-darwin");
        assert!(matches!(&p.source, Source::Git { image: None, deps, .. } if deps.is_empty()));
        assert!(p.build[2].starts_with("cd \"$FSPEC_SRC\"") && !p.build[2].contains("musl"));
        assert_eq!(build_for("x86_64-linux"), pkg());
    }

    #[test]
    fn the_engine_tracks_main_and_links_the_fixed_path() {
        let p = pkg();
        assert!(matches!(&p.source, Source::Git { url, rev, .. } if url == REPO && rev == "main"));
        assert_eq!(p.files[0].path, BIN);
        assert_eq!(p.files[0].target.as_deref(), Some(LINK));
    }

    fn host() -> crate::Host {
        serde_json::from_value(serde_json::json!({
            "name": "h", "platform": "x86_64-linux", "resources": [Resource::Pkg(pkg())],
        }))
        .unwrap()
    }

    #[test]
    fn an_unlocked_engine_is_left_out_with_its_link() {
        assert!(unless_unlocked(host().desired(), &Lock::default()).is_empty());
    }

    #[test]
    fn a_locked_engine_stays() {
        let lock: Lock = toml::from_str(&format!(
            "[[pkg]]\nname = \"fspec\"\nsource = \"{}\"\nrev = \"{}\"\n",
            pkg().source,
            "a".repeat(40)
        ))
        .unwrap();
        assert_eq!(unless_unlocked(host().desired(), &lock).len(), 2);
    }
}
