use std::fs;

use fspec_core::Scope;
use fspec_core::apply::{Backend, apply_scope, plan_scope};
use fspec_core::facts::Init;
use fspec_core::plan::Action;
use fspec_core::runner::Output;
use fspec_core::state::State;
use serde_json::json;

use super::*;
use crate::testutil::{World, desired, resource};

fn kyan(shell: &str) -> Resource {
    resource(json!({"kind": "user", "name": "kyan", "uid": 1000, "shell": shell, "groups": ["wheel", "docker"]}))
}

fn out(stdout: &str) -> Output {
    Output { stdout: stdout.into(), ..Output::default() }
}

#[test]
fn creates_a_linux_account_with_a_shell_from_etc_shells() {
    let w = World::linux(Init::Systemd);
    fs::create_dir_all(w.paths.root.join("etc")).unwrap();
    fs::write(w.paths.root.join("etc/shells"), "# shells\n/bin/bash\n/usr/local/bin/fish\n").unwrap();
    apply_scope(&w.cx(Scope::System), &[&UsersBackend], &[desired(kyan("fish"))]).unwrap();
    assert_eq!(
        w.runner.lines(),
        [
            "getent passwd kyan",
            "useradd -m -u 1000 -s /usr/local/bin/fish kyan",
            "getent passwd kyan",
            "id -nG kyan",
            "getent group wheel",
            "getent group docker",
            "usermod -aG wheel,docker kyan",
        ]
    );
}

/// Installs every package with one zypper call, the way a distro backend would.
struct Zypper;

impl Backend for Zypper {
    fn name(&self) -> &'static str {
        "zypper"
    }
    fn handles(&self, _cx: &Cx, r: &Resource) -> bool {
        matches!(r, Resource::Pkg(_))
    }
    fn phase(&self) -> Phase {
        Phase::Packages
    }
    fn commit(&self, cx: &Cx, ops: &[Op], _staging: &Path, _journal: &mut Journal) -> Result<Records> {
        cx.runner.check(&Cmd::new("zypper").args(["install", "docker"]))?;
        Ok(ops.iter().map(|o| (o.key.clone(), Map::new())).collect())
    }
}

#[test]
fn groups_are_joined_after_the_packages_that_create_them() {
    let w = World::linux(Init::Systemd);
    let docker = resource(json!({"kind": "pkg", "name": "docker", "source": {"kind": "dnf"}}));
    apply_scope(&w.cx(Scope::System), &[&UsersBackend, &Zypper], &[desired(kyan("bash")), desired(docker)]).unwrap();
    let lines = w.runner.lines();
    let at = |l: &str| lines.iter().position(|x| x.starts_with(l)).unwrap_or_else(|| panic!("{l}: {lines:?}"));
    assert!(at("useradd") < at("zypper install docker"), "{lines:?}");
    assert!(at("zypper install docker") < at("usermod -aG wheel,docker kyan"), "{lines:?}");
}

#[test]
fn shell_falls_back_to_usr_bin() {
    let w = World::linux(Init::Systemd);
    assert_eq!(resolve_shell(&w.cx(Scope::System), "fish"), "/usr/bin/fish");
    assert_eq!(resolve_shell(&w.cx(Scope::System), "/opt/x/sh"), "/opt/x/sh");
}

#[test]
fn adjusts_an_existing_linux_account() {
    let w = World::linux(Init::Systemd);
    let backends: &[&dyn Backend] = &[&UsersBackend];
    apply_scope(&w.cx(Scope::System), backends, &[desired(kyan("bash"))]).unwrap();
    w.runner.calls.lock().unwrap().clear();

    w.runner.respond("getent passwd kyan", out("kyan:x:1000:1000::/home/kyan:/usr/bin/bash\n"));
    w.runner.respond("id -nG kyan", out("kyan wheel\n"));
    apply_scope(&w.cx(Scope::System), backends, &[desired(kyan("fish"))]).unwrap();
    assert_eq!(
        w.runner.lines(),
        [
            "getent passwd kyan",
            "getent passwd kyan",
            "usermod -s /usr/bin/fish kyan",
            "id -nG kyan",
            "getent group docker",
            "usermod -aG docker kyan",
        ]
    );
}

#[test]
fn removal_keeps_the_account() {
    let w = World::linux(Init::Systemd);
    let backends: &[&dyn Backend] = &[&UsersBackend];
    apply_scope(&w.cx(Scope::System), backends, &[desired(kyan("bash"))]).unwrap();
    w.runner.calls.lock().unwrap().clear();
    apply_scope(&w.cx(Scope::System), backends, &[]).unwrap();
    assert!(w.runner.lines().is_empty());
    let state = State::load(&w.paths.system_state_dir().join("state.json")).unwrap();
    assert!(state.entries.is_empty());
}

#[test]
fn failed_usermod_undoes_the_earlier_change() {
    let w = World::linux(Init::Systemd);
    let backends: &[&dyn Backend] = &[&UsersBackend];
    apply_scope(&w.cx(Scope::System), backends, &[desired(kyan("bash"))]).unwrap();
    w.runner.calls.lock().unwrap().clear();

    w.runner.respond("getent passwd kyan", out("kyan:x:1000:1000::/home/kyan:/usr/bin/bash\n"));
    w.runner.respond("id -nG kyan", out("kyan\n"));
    w.runner.respond("usermod -aG", Output { status: 6, ..Output::default() });
    apply_scope(&w.cx(Scope::System), backends, &[desired(kyan("fish"))]).unwrap_err();
    let lines = w.runner.lines();
    assert_eq!(lines[2], "usermod -s /usr/bin/fish kyan");
    assert!(lines.contains(&"usermod -s /usr/bin/bash kyan".to_string()), "{lines:?}");
    assert!(lines.contains(&"gpasswd -d kyan wheel".to_string()), "{lines:?}");
}

#[test]
fn drift_on_linux() {
    let w = World::linux(Init::Systemd);
    let backends: &[&dyn Backend] = &[&UsersBackend];
    let want = vec![desired(kyan("bash"))];
    apply_scope(&w.cx(Scope::System), backends, &want).unwrap();
    let plan = |w: &World| {
        let state = State::load(&w.paths.system_state_dir().join("state.json")).unwrap();
        plan_scope(&w.cx(Scope::System), backends, &want, &state).unwrap()
    };
    // A dry run answers nothing, which is not drift.
    assert!(plan(&w).changes.is_empty());

    w.runner.respond("getent passwd kyan", out("kyan:x:1001:1000::/home/kyan:/bin/zsh\n"));
    w.runner.respond("id -nG kyan", out("kyan wheel\n"));
    let p = plan(&w);
    let Action::Drift { notes } = &p.changes[0].action else { panic!("{p:?}") };
    assert_eq!(notes, &["uid is 1001", "shell is /bin/zsh", "not in group docker"]);

    let missing = World::linux(Init::Systemd);
    apply_scope(&missing.cx(Scope::System), backends, &want).unwrap();
    missing.runner.respond("getent passwd kyan", Output { status: 2, ..Output::default() });
    let state = State::load(&missing.paths.system_state_dir().join("state.json")).unwrap();
    let p = plan_scope(&missing.cx(Scope::System), backends, &want, &state).unwrap();
    assert!(matches!(&p.changes[0].action, Action::Drift { notes } if notes == &["account kyan is missing"]));
}

fn mac_user(shell: &str) -> Resource {
    resource(json!({"kind": "user", "name": "kyan", "uid": 501, "shell": shell}))
}

#[test]
fn macos_only_changes_the_shell() {
    let w = World::darwin();
    w.runner.respond("dscl . -read /Users/kyan UserShell", out("UserShell: /bin/zsh\n"));
    w.runner.respond("dscl . -read /Users/kyan UniqueID", out("UniqueID: 501\n"));
    apply_scope(&w.cx(Scope::System), &[&UsersBackend], &[desired(mac_user("/opt/homebrew/bin/fish"))]).unwrap();
    assert_eq!(
        w.runner.lines(),
        [
            "dscl . -read /Users/kyan",
            "dscl . -read /Users/kyan UniqueID",
            "dscl . -read /Users/kyan UserShell",
            "chsh -s /opt/homebrew/bin/fish kyan",
        ]
    );
}

#[test]
fn macos_leaves_a_matching_shell_alone() {
    let w = World::darwin();
    w.runner.respond("dscl . -read /Users/kyan UserShell", out("UserShell: /bin/zsh\n"));
    apply_scope(&w.cx(Scope::System), &[&UsersBackend], &[desired(mac_user("/bin/zsh"))]).unwrap();
    assert!(!w.runner.lines().iter().any(|l| l.starts_with("chsh")));
}

#[test]
fn macos_never_creates_or_renumbers_accounts() {
    let w = World::darwin();
    w.runner.respond("dscl . -read /Users/kyan", Output { status: 56, ..Output::default() });
    let err = apply_scope(&w.cx(Scope::System), &[&UsersBackend], &[desired(mac_user("/bin/zsh"))]).unwrap_err();
    assert!(format!("{err:#}").contains("never creates macOS accounts"), "{err:#}");

    let w = World::darwin();
    w.runner.respond("dscl . -read /Users/kyan UniqueID", out("UniqueID: 502\n"));
    let err = apply_scope(&w.cx(Scope::System), &[&UsersBackend], &[desired(mac_user("/bin/zsh"))]).unwrap_err();
    assert!(format!("{err:#}").contains("uid 502 but the spec says 501"), "{err:#}");
}

#[test]
fn check_rejects_macos_groups_and_bad_names() {
    let w = World::darwin();
    let err = UsersBackend.check(&w.cx(Scope::System), &kyan("zsh")).unwrap_err().to_string();
    assert!(err.contains("only manages the login shell"), "{err}");
    let bad = resource(json!({"kind": "user", "name": "-x"}));
    assert!(UsersBackend.check(&World::linux(Init::Systemd).cx(Scope::System), &bad).is_err());
}

#[test]
fn a_group_that_does_not_exist_yet_waits_for_a_later_apply() {
    let w = World::linux(Init::Systemd);
    w.runner.respond("getent group nordvpn", Output { status: 2, ..Output::default() });
    let mut u = kyan("bash");
    if let Resource::User(user) = &mut u {
        user.groups.push("nordvpn".into());
    }
    apply_scope(&w.cx(Scope::System), &[&UsersBackend], &[desired(u)]).unwrap();
    let lines = w.runner.lines();
    assert!(lines.contains(&"usermod -aG wheel,docker kyan".to_string()), "{lines:?}");
    let at = |l: &str| lines.iter().position(|x| x == l).unwrap();
    assert!(
        at("getent group nordvpn") < at("systemd-sysusers")
            && at("systemd-sysusers") < at("usermod -aG wheel,docker kyan")
    );
}

fn subid_files(w: &World, subuid: &str, subgid: &str) {
    fs::create_dir_all(w.paths.root.join("etc")).unwrap();
    fs::write(w.paths.root.join("etc/subuid"), subuid).unwrap();
    fs::write(w.paths.root.join("etc/subgid"), subgid).unwrap();
}

#[test]
fn an_account_without_a_subid_range_gets_the_first_free_block() {
    let w = World::linux(Init::Systemd);
    subid_files(&w, "dockremap:100000000:100000001\nold:100000:70000\n", "dockremap:100000000:100000001\n");
    w.runner.respond("getent passwd kyan", out("kyan:x:1000:1000::/home/kyan:/usr/bin/bash\n"));
    w.runner.respond("id -nG kyan", out("kyan wheel docker\n"));
    apply_scope(&w.cx(Scope::System), &[&UsersBackend], &[desired(kyan("bash"))]).unwrap();
    let lines = w.runner.lines();
    assert!(
        lines.contains(&"usermod --add-subuids 231072-296607 --add-subgids 231072-296607 kyan".to_string()),
        "{lines:?}"
    );
}

#[test]
fn an_existing_subid_range_is_left_alone() {
    let w = World::linux(Init::Systemd);
    subid_files(&w, "kyan:100000:65536\n", "1000:100000:65536\n");
    w.runner.respond("getent passwd kyan", out("kyan:x:1000:1000::/home/kyan:/usr/bin/bash\n"));
    w.runner.respond("id -nG kyan", out("kyan wheel docker\n"));
    apply_scope(&w.cx(Scope::System), &[&UsersBackend], &[desired(kyan("bash"))]).unwrap();
    assert!(!w.runner.lines().iter().any(|l| l.contains("subuids") || l.contains("subgids")));

    let w = World::linux(Init::Systemd);
    subid_files(&w, "kyan:100000:65536\n", "");
    w.runner.respond("getent passwd kyan", out("kyan:x:1000:1000::/home/kyan:/usr/bin/bash\n"));
    w.runner.respond("id -nG kyan", out("kyan wheel\n"));
    w.runner.respond("usermod -aG", Output { status: 6, ..Output::default() });
    apply_scope(&w.cx(Scope::System), &[&UsersBackend], &[desired(kyan("bash"))]).unwrap_err();
    let lines = w.runner.lines();
    assert!(lines.contains(&"usermod --add-subgids 165536-231071 kyan".to_string()), "{lines:?}");
    assert!(lines.contains(&"usermod --del-subgids 165536-231071 kyan".to_string()), "{lines:?}");
}

#[test]
fn drift_shows_a_missing_subid_range() {
    let w = World::linux(Init::Systemd);
    let backends: &[&dyn Backend] = &[&UsersBackend];
    let want = vec![desired(kyan("bash"))];
    apply_scope(&w.cx(Scope::System), backends, &want).unwrap();
    subid_files(&w, "kyan:100000:65536\n", "dockremap:100000000:100000001\n");
    w.runner.respond("getent passwd kyan", out("kyan:x:1000:1000::/home/kyan:/usr/bin/bash\n"));
    w.runner.respond("id -nG kyan", out("kyan wheel docker\n"));
    let state = State::load(&w.paths.system_state_dir().join("state.json")).unwrap();
    let p = plan_scope(&w.cx(Scope::System), backends, &want, &state).unwrap();
    assert!(matches!(&p.changes[0].action, Action::Drift { notes } if notes == &["no range in /etc/subgid"]), "{p:?}");
}
