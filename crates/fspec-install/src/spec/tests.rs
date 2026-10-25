use std::path::PathBuf;

use fspec_core::resource::{Resource, System};

use super::*;
use crate::catalog::DISTROS;
use crate::model::{Account, Desktop, DiskChoice, Distro, Locale, Secret};

fn locale() -> Locale {
    Locale { locale: "en_US.UTF-8".into(), keymap: "us".into(), timezone: "Europe/Madrid".into() }
}

fn account() -> Account {
    Account { name: "kyan".into(), full_name: "Kyan".into(), hostname: "lab".into(), password: Secret("x".into()) }
}

fn disk() -> DiskChoice {
    DiskChoice { device: "/dev/vda".into(), encrypt: None, swap_gib: 8, windows: None }
}

fn choices(distro: &'static str, preset: Preset, desktop: Option<Desktop>) -> Choices {
    Choices {
        locale: locale(),
        disk: disk(),
        system: SystemChoice::Fresh { distro, preset, desktop },
        account: account(),
        secure_boot: false,
    }
}

fn combinations() -> Vec<(&'static Distro, Preset, Option<Desktop>)> {
    let mut all = Vec::new();
    for d in DISTROS.iter() {
        for &p in d.presets {
            if matches!(p, Preset::Desktop | Preset::Gaming) {
                all.extend(d.desktops.iter().map(|&de| (d, p, Some(de))));
            } else {
                all.push((d, p, None));
            }
        }
    }
    all
}

fn services(host: &fspec_core::resource::Host) -> Vec<&str> {
    host.resources
        .iter()
        .filter_map(|r| match r {
            Resource::Service(s) => Some(s.name.as_str()),
            _ => None,
        })
        .collect()
}

fn packages(host: &fspec_core::resource::Host) -> Vec<&str> {
    host.resources
        .iter()
        .filter_map(|r| match r {
            Resource::Pkg(p) => Some(p.name.as_str()),
            _ => None,
        })
        .collect()
}

#[test]
fn every_combination_writes_a_spec_that_evaluates() {
    for (distro, preset, desktop) in combinations() {
        let what = format!("{} {} {:?}", distro.id, preset.id(), desktop);
        let dir = tempfile::tempdir().unwrap();
        write_repo(dir.path(), &choices(distro.id, preset, desktop)).unwrap();
        let spec = fspec_eval::evaluate(&dir.path().join("spec.star")).unwrap_or_else(|e| panic!("{what}: {e:#}"));
        let host = spec.host("lab").expect(&what);
        assert_eq!(host.users.len(), 1, "{what}");
        assert_eq!(host.users[0].name, "kyan", "{what}");
        assert_eq!(host.users[0].uid, Some(1000), "{what}");
        let admin = if matches!(distro.id, "debian" | "ubuntu") { "sudo" } else { "wheel" };
        assert_eq!(host.users[0].groups, [admin], "{what}");
        assert_eq!(host.users[0].shell.as_deref(), Some("bash"), "{what}");
        assert_eq!(host.protect, ["/home"], "{what}");
        assert_eq!(host.locale.as_deref(), Some("en_US.UTF-8"), "{what}");
        assert_eq!(host.timezone.as_deref(), Some("Europe/Madrid"), "{what}");

        let svc = services(host);
        match &host.system {
            Some(System::Distro(d)) => {
                assert_eq!(d.distro, distro.id, "{what}");
                assert!(fspec_switch::DISTROS.contains(&d.distro.as_str()), "{what}: disk init cannot bootstrap it");
                fspec_switch::recipe(&d.distro).expect(&what);
                assert!(!d.packages.is_empty(), "{what}");
                assert!(svc.contains(&"NetworkManager"), "{what}: {svc:?}");
                let dm = match (desktop, distro.id) {
                    (Some(Desktop::Gnome), "debian" | "ubuntu") => Some("gdm3"),
                    (Some(Desktop::Gnome), _) => Some("gdm"),
                    (Some(Desktop::Kde), _) => Some("sddm"),
                    (Some(Desktop::Cosmic), _) => Some("cosmic-greeter"),
                    (None, _) => None,
                };
                if let Some(dm) = dm {
                    assert!(svc.contains(&dm), "{what}: {svc:?}");
                    assert!(svc.iter().any(|s| s.starts_with("cups")), "{what}: {svc:?}");
                    assert!(packages(host).contains(&"org.mozilla.firefox"), "{what}");
                }
                if preset == Preset::Gaming {
                    assert!(packages(host).contains(&"com.valvesoftware.Steam"), "{what}");
                    assert!(d.packages.iter().any(|p| p == "gamemode"), "{what}");
                }
                if preset == Preset::Server {
                    assert!(svc.iter().any(|s| s.starts_with("ssh")), "{what}: {svc:?}");
                    assert!(svc.iter().any(|s| matches!(*s, "firewalld" | "nftables")), "{what}: {svc:?}");
                }
            }
            other => panic!("{what}: unexpected system {other:?}"),
        }
    }
}

#[test]
fn the_repo_is_a_git_repo_with_one_commit() {
    let dir = tempfile::tempdir().unwrap();
    write_repo(dir.path(), &choices("fedora", Preset::Desktop, Some(Desktop::Gnome))).unwrap();
    let log = Command::new("git").arg("-C").arg(dir.path()).args(["log", "--format=%an <%ae>"]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&log.stdout), "FormalSpec installer <installer@formalspec.invalid>\n");
    let status = Command::new("git").arg("-C").arg(dir.path()).args(["status", "--porcelain"]).output().unwrap();
    assert!(status.stdout.is_empty());
}

#[test]
fn preview_of_a_complete_draft_is_the_written_spec() {
    let c = choices("debian", Preset::Gaming, Some(Desktop::Kde));
    let draft = Draft {
        locale: Some(c.locale.clone()),
        disk: Some(c.disk.clone()),
        system: Some(c.system.clone()),
        account: Some(c.account.clone()),
        secure_boot: false,
    };
    let written = files(&c).into_iter().find(|(p, _)| p == &PathBuf::from("spec.star")).unwrap().1;
    assert_eq!(preview(&draft), written);
    assert!(written.contains("groups = [\"sudo\"]"));
}

#[test]
fn preview_only_grows_as_the_draft_fills_in() {
    let c = choices("fedora", Preset::Desktop, Some(Desktop::Gnome));
    let mut d = Draft::default();
    let mut steps = vec![preview(&d)];
    d.locale = Some(c.locale.clone());
    steps.push(preview(&d));
    d.system = Some(c.system.clone());
    steps.push(preview(&d));
    d.disk = Some(c.disk.clone());
    steps.push(preview(&d));
    d.account = Some(c.account.clone());
    steps.push(preview(&d));
    for pair in steps.windows(2) {
        let (before, after) = (&pair[0], &pair[1]);
        let mut rest = after.lines();
        for line in before.lines() {
            assert!(rest.any(|l| l == line), "`{line}` vanished or moved:\n{before}\n--\n{after}");
        }
        assert!(after.len() > before.len() || before == after);
    }
    assert!(steps[0].lines().count() < steps[4].lines().count());
}

#[test]
fn a_restore_writes_nothing() {
    let c = Choices {
        system: SystemChoice::Restore {
            url: "https://example.invalid/spec.git".into(),
            auth: crate::model::GitAuth::None,
            checkout: "/tmp/spec".into(),
            host: "lab".into(),
        },
        ..choices("fedora", Preset::Minimal, None)
    };
    assert!(files(&c).is_empty());
    assert!(write_repo(Path::new("/nonexistent"), &c).is_err());
}

/// The first apply runs in the new root with the repo exactly as written, before any `fspec
/// update`, so nothing it declares may need a lock entry the repo does not have.
#[test]
fn every_combination_applies_without_a_lock() {
    use fspec_core::resource::Source;
    for (distro, preset, desktop) in combinations() {
        let what = format!("{} {} {:?}", distro.id, preset.id(), desktop);
        let dir = tempfile::tempdir().unwrap();
        write_repo(dir.path(), &choices(distro.id, preset, desktop)).unwrap();
        let spec = fspec_eval::evaluate(&dir.path().join("spec.star")).unwrap();
        let lock = fspec_core::Lock::load(&dir.path().join("fspec.lock")).unwrap();
        for d in fspec_core::engine::unless_unlocked(spec.host("lab").unwrap().desired(), &lock) {
            let Resource::Pkg(p) = &d.resource else { continue };
            // What the store installs, and only from a lock entry.
            let stored = matches!(
                p.source,
                Source::Aqua { .. }
                    | Source::Github { .. }
                    | Source::Npm { .. }
                    | Source::Cargo { .. }
                    | Source::Git { .. }
                    | Source::Nix { .. }
                    | Source::Url { .. }
                    | Source::Python
            );
            assert!(
                !stored || lock.pkg(&p.name, &p.source.to_string()).is_some(),
                "{what}: pkg {} needs fspec.lock",
                p.name
            );
        }
    }
}
