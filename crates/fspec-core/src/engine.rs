//! The fspec every Linux host runs. It is a `git(...)` package the evaluator adds to each Linux
//! host itself, so `fspec update` locks it, CI builds it and `fspec apply` installs it like any
//! other package, and no spec can leave it out.

use crate::resource::{Pkg, Resource};

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

pub fn pkg() -> Pkg {
    serde_json::from_value(serde_json::json!({
        "name": NAME,
        "source": {"kind": "git", "url": REPO, "rev": "main", "image": IMAGE, "deps": ["build-base"]},
        "build": ["sh", "-c", BUILD],
        "bins": [NAME],
        "files": [{"path": BIN, "mode": "link", "target": LINK}],
    }))
    .expect("the engine package is a valid pkg")
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
    use crate::resource::Source;

    #[test]
    fn the_engine_tracks_main_and_links_the_fixed_path() {
        let p = pkg();
        assert!(matches!(&p.source, Source::Git { url, rev, .. } if url == REPO && rev == "main"));
        assert_eq!(p.files[0].path, BIN);
        assert_eq!(p.files[0].target.as_deref(), Some(LINK));
    }
}
