//! Command construction for remote hosts. Pure, so the exact argument lists are unit tested.

use std::path::Path;

use fspec_core::Host;
use fspec_core::runner::Cmd;

#[derive(Debug, PartialEq, Eq)]
pub enum Target {
    Local,
    Remote { alias: String },
}

/// A host is remote unless it is named after this machine.
pub fn target(host: &Host, local_name: &str) -> Target {
    if host.name.eq_ignore_ascii_case(local_name) {
        Target::Local
    } else {
        Target::Remote { alias: host.ssh.clone().unwrap_or_else(|| host.name.clone()) }
    }
}

/// Where a shipped spec lands on the remote, as a shell word. A dirty tree's contents are not
/// pinned by the commit, so its directory is named after a hash of what is shipped.
pub fn spec_dir(commit: &str, dirty: Option<&str>) -> String {
    let leaf = match dirty {
        Some(hash) => format!("{commit}-dirty-{hash}"),
        None => commit.to_string(),
    };
    format!("\"$HOME/.cache/fspec/spec/{leaf}\"")
}

/// fspec as the remote's shell finds it. `fspec switch`, `fspec disk init` and the engine upgrade
/// install it in /usr/local/bin, which AerynOS's non-interactive ssh PATH leaves out. It goes
/// first: a store-managed fspec under /opt/fspec/current/bin is the lock's older one until the
/// apply the upgrade unblocks has run.
pub const FSPEC: &str = r#"PATH="/usr/local/bin:$PATH" fspec"#;

pub fn batch(alias: &str) -> Cmd {
    Cmd::new("ssh").args(["-o", "ConnectTimeout=5", "-o", "BatchMode=yes"]).arg(alias)
}

pub fn probe(alias: &str) -> Cmd {
    batch(alias).arg(format!("{FSPEC} facts --json"))
}

pub fn git_commit(spec_dir: &Path) -> Cmd {
    Cmd::new("git").arg("-C").arg(spec_dir.display().to_string()).args(["rev-parse", "HEAD"])
}

pub fn git_status(spec_dir: &Path) -> Cmd {
    Cmd::new("git").arg("-C").arg(spec_dir.display().to_string()).args(["status", "--porcelain", "--", "."])
}

/// Paths relative to the spec dir, which `tar_create` archives from, even when the spec lives in
/// a subdirectory of its repo.
pub fn git_files(spec_dir: &Path) -> Cmd {
    Cmd::new("git").arg("-C").arg(spec_dir.display().to_string()).args(["ls-files", "--", "."])
}

pub fn git_untracked(spec_dir: &Path) -> Cmd {
    Cmd::new("git").arg("-C").arg(spec_dir.display().to_string()).args([
        "ls-files",
        "--others",
        "--exclude-standard",
        "--",
        ".",
    ])
}

/// Archives the listed files (one path per line on stdin) relative to the spec dir.
pub fn tar_create(spec_dir: &Path, archive: &Path, files: &[String]) -> Cmd {
    Cmd::new("tar")
        .args(["-cf", &archive.display().to_string(), "-C", &spec_dir.display().to_string(), "-T", "-"])
        .stdin(files.join("\n"))
}

/// Unpacks `archive` into `dir` on the remote, beside what is already there.
pub fn ship_into(alias: &str, dir: &str, archive: &Path) -> Cmd {
    batch(alias).arg(format!("mkdir -p {dir} && tar -xf - -C {dir}")).stdin_file(archive)
}

/// A dirty ship first drops the earlier dirty copies of the same commit. The login shell can be
/// fish, which fails a glob that matches nothing, so `find` does the matching.
pub fn ship(alias: &str, commit: &str, dest: &str, archive: &Path, dirty: bool) -> Cmd {
    let wipe = if dirty {
        format!(
            "find \"$HOME/.cache/fspec/spec\" -maxdepth 1 -name '{commit}-dirty*' -exec rm -rf '{{}}' + 2>/dev/null; "
        )
    } else {
        String::new()
    };
    batch(alias).arg(format!("{wipe}mkdir -p {dest} && tar -xf - -C {dest}")).stdin_file(archive)
}

fn fspec(dest: &str, spec_file: &str, args: &[&str]) -> String {
    format!("{FSPEC} --spec {dest}/{spec_file} {}", args.join(" "))
}

pub fn apply(alias: &str, dest: &str, spec_file: &str, host: &str, scope: Option<&str>) -> Cmd {
    let mut args = vec!["apply", "--local", "--host", host];
    if let Some(scope) = scope {
        args.extend(["--scope", scope]);
    }
    Cmd::new("ssh").args(["-t", alias]).arg(fspec(dest, spec_file, &args)).interactive()
}

/// Where [`upload_engine`] leaves the engine for [`install_engine`] to move into place.
const ENGINE_UPLOAD: &str = "\"$HOME/.cache/fspec/engine.new\"";

pub fn upload_engine(alias: &str, file: &Path) -> Cmd {
    batch(alias).arg(format!("mkdir -p \"$HOME/.cache/fspec\" && cat > {ENGINE_UPLOAD}")).stdin_file(file)
}

/// Interactive, since sudo may ask for a password.
pub fn install_engine(alias: &str) -> Cmd {
    let bin = fspec_core::engine::BIN;
    Cmd::new("ssh")
        .args(["-t", alias])
        .arg(format!("sudo install -D -m 0755 {ENGINE_UPLOAD} {bin} && rm -f {ENGINE_UPLOAD}"))
        .interactive()
}

pub fn plan(alias: &str, dest: &str, spec_file: &str, host: &str) -> Cmd {
    batch(alias).arg(fspec(dest, spec_file, &["plan", "--json", "--local", "--host", host]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(name: &str, ssh: Option<&str>) -> Host {
        serde_json::from_value(serde_json::json!({"name": name, "platform": "x86_64-linux", "ssh": ssh})).unwrap()
    }

    #[test]
    fn local_when_named_after_this_machine() {
        assert_eq!(target(&host("macbook", None), "macbook"), Target::Local);
        assert_eq!(target(&host("MacBook", None), "macbook"), Target::Local);
    }

    #[test]
    fn remote_uses_ssh_alias_or_host_name() {
        assert_eq!(target(&host("g815", None), "macbook"), Target::Remote { alias: "g815".into() });
        assert_eq!(
            target(&host("g815", Some("kyan@g815.lan")), "macbook"),
            Target::Remote { alias: "kyan@g815.lan".into() }
        );
    }

    #[test]
    fn probe_line() {
        assert_eq!(
            probe("g815").to_string(),
            r#"ssh -o ConnectTimeout=5 -o BatchMode=yes g815 'PATH="/usr/local/bin:$PATH" fspec facts --json'"#
        );
    }

    #[test]
    fn git_lines() {
        let d = Path::new("/r/spec");
        assert_eq!(git_commit(d).to_string(), "git -C /r/spec rev-parse HEAD");
        assert_eq!(git_status(d).to_string(), "git -C /r/spec status --porcelain -- .");
        assert_eq!(git_files(d).to_string(), "git -C /r/spec ls-files -- .");
    }

    #[test]
    fn tar_and_ship_lines() {
        let tar = tar_create(Path::new("/r/spec"), Path::new("/t/s.tar"), &["spec.star".into(), "a/b.star".into()]);
        assert_eq!(tar.to_string(), "tar -cf /t/s.tar -C /r/spec -T -");
        assert_eq!(tar.stdin.as_deref(), Some("spec.star\na/b.star"));

        let dest = spec_dir("abc123", None);
        let ship = ship("g815", "abc123", &dest, Path::new("/t/s.tar"), false);
        assert_eq!(
            ship.args,
            [
                "-o",
                "ConnectTimeout=5",
                "-o",
                "BatchMode=yes",
                "g815",
                "mkdir -p \"$HOME/.cache/fspec/spec/abc123\" && tar -xf - -C \"$HOME/.cache/fspec/spec/abc123\""
            ]
        );
        assert_eq!(ship.stdin_file.as_deref(), Some(Path::new("/t/s.tar")));
    }

    #[test]
    fn dirty_ship_wipes_first() {
        let dest = spec_dir("abc123", Some("0a1b2c3d4e5f"));
        let ship = ship("g815", "abc123", &dest, Path::new("/t/s.tar"), true);
        assert_eq!(
            ship.args.last().unwrap(),
            "find \"$HOME/.cache/fspec/spec\" -maxdepth 1 -name 'abc123-dirty*' -exec rm -rf '{}' + 2>/dev/null; mkdir -p \"$HOME/.cache/fspec/spec/abc123-dirty-0a1b2c3d4e5f\" && tar -xf - -C \"$HOME/.cache/fspec/spec/abc123-dirty-0a1b2c3d4e5f\""
        );
    }

    #[test]
    fn apply_and_plan_lines() {
        let dest = spec_dir("abc123", None);
        let apply = apply("g815", &dest, "spec.star", "g815", None);
        assert_eq!(apply.program, "ssh");
        assert_eq!(
            apply.args,
            [
                "-t",
                "g815",
                r#"PATH="/usr/local/bin:$PATH" fspec --spec "$HOME/.cache/fspec/spec/abc123"/spec.star apply --local --host g815"#
            ]
        );
        assert!(apply.interactive);
        let scoped = super::apply("g815", &dest, "spec.star", "g815", Some("user"));
        assert!(scoped.args[2].ends_with("apply --local --host g815 --scope user"));
        assert_eq!(
            plan("g815", &dest, "spec.star", "g815").args[5],
            r#"PATH="/usr/local/bin:$PATH" fspec --spec "$HOME/.cache/fspec/spec/abc123"/spec.star plan --json --local --host g815"#
        );
    }
}
