//! `fspec search`: the catalog search the launcher's Install mode and an unresolved name's
//! suggestions share (`fspec_sources::catalog`).

use std::collections::BTreeSet;
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::Result;
use fspec_core::fetch::LiveFetch;
use fspec_core::resource::Pkg;
use fspec_core::runner::{Cmd, LiveRunner, Runner};
use fspec_core::{Paths, Resource, Scope, State};
use fspec_sources::catalog::{self, Catalog, Hit, Origin};
use serde_json::json;

/// Per-query catalogs answer within this or are skipped.
const QUICK: Duration = Duration::from_secs(2);
/// The aqua registry is 3.5 MB, fetched at most once a day.
const INDEX: Duration = Duration::from_secs(15);
/// Flathub's AppStream catalog is fetched again in the background once it is older than this.
const APPSTREAM_FRESH: Duration = Duration::from_secs(24 * 3600);
const FLATHUB: &str = "https://dl.flathub.org/repo/flathub.flatpakrepo";

pub fn catalog(paths: &Paths, network: bool) -> Catalog {
    let net = network.then(|| Box::new(LiveFetch::with_timeout(Some(QUICK))) as Box<dyn fspec_core::fetch::Fetch>);
    Catalog::local(&paths.home, net)
}

/// Flathub's name for an app, when a local AppStream catalog has it.
pub fn flathub_title(paths: &Paths, id: &str) -> Option<String> {
    let cat = catalog(paths, false);
    let active = fspec_sources::appstream::newest(&cat.appstream)?;
    let apps = fspec_sources::appstream::apps(&active, &cat.cache.join("flathub-apps.json")).ok()?;
    apps.into_iter().find(|a| a.id == id)?.name
}

/// Fetch Flathub's AppStream catalog into fspec's own flatpak installation when none is there or
/// the newest is a day old, in a process of its own so the search that asked answers at once
/// from what it has. A stamp keeps a burst of keystrokes from starting more than one.
fn refresh_appstream(cat: &Catalog, home: &Path) {
    let age = |p: &Path| {
        std::fs::metadata(p).and_then(|m| m.modified()).ok().and_then(|t| SystemTime::now().duration_since(t).ok())
    };
    let newest = fspec_sources::appstream::newest(&cat.appstream);
    if newest.is_some_and(|d| age(&d.join("appstream.xml.gz")).is_some_and(|a| a < APPSTREAM_FRESH)) {
        return;
    }
    let stamp = cat.cache.join("flathub-appstream.refresh");
    if age(&stamp).is_some_and(|a| a < Duration::from_secs(600)) {
        return;
    }
    if std::fs::create_dir_all(&cat.cache).is_err() || std::fs::write(&stamp, "").is_err() {
        return;
    }
    use std::os::unix::process::CommandExt;
    let script = format!(
        "flatpak --user remote-add --if-not-exists flathub {FLATHUB} && flatpak --user update --appstream flathub"
    );
    let mut c = std::process::Command::new("sh");
    c.args(["-c", &script])
        .env("FLATPAK_USER_DIR", catalog::fspec_flatpak(home))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // SAFETY: setsid is async-signal-safe and touches no memory of the parent.
    unsafe {
        c.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let _ = c.spawn();
}

/// Who installed each hit on this machine: the spec (a package in either state file), Flatpak or
/// the distro's package manager. Asked once per source for every hit shown.
fn owners(paths: &Paths, hits: &[&Hit], runner: &dyn Runner) -> Vec<Option<&'static str>> {
    let mut declared: Vec<Pkg> = vec![];
    for scope in [Scope::System, Scope::User] {
        let state = State::load(&fspec_core::apply::state_dir(paths, scope).join("state.json")).unwrap_or_default();
        declared.extend(state.entries.into_values().filter_map(|e| match e.resource {
            Resource::Pkg(p) => Some(p),
            _ => None,
        }));
    }
    let names = |o: Origin| hits.iter().filter(|h| h.origin == o).map(|h| h.name.clone()).collect::<Vec<_>>();
    let flatpaks: BTreeSet<String> = match names(Origin::Flathub).is_empty() {
        true => BTreeSet::new(),
        false => runner
            .run(&Cmd::new("flatpak").args(["list", "--app", "--columns=application"]))
            .map(|o| o.stdout.lines().map(|l| l.trim().to_string()).collect())
            .unwrap_or_default(),
    };
    let mut distro: BTreeSet<String> = BTreeSet::new();
    for (origin, cmd) in [
        (Origin::Zypper, Cmd::new("rpm").args(["-q", "--qf", "%{NAME}\\n"])),
        (Origin::Pacman, Cmd::new("pacman").arg("-Qq")),
        (Origin::Apt, Cmd::new("dpkg-query").args(["-W", "-f", "${Package} ${db:Status-Status}\\n"])),
    ] {
        let wanted = names(origin);
        if wanted.is_empty() {
            continue;
        }
        let Ok(out) = runner.run(&cmd.arg("--").args(wanted.clone())) else { continue };
        for line in out.stdout.lines() {
            let (name, status) = line.split_once(' ').unwrap_or((line, "installed"));
            if wanted.iter().any(|w| w == name) && status == "installed" {
                distro.insert(name.to_string());
            }
        }
    }
    hits.iter()
        .map(|h| {
            let source = source_of(h);
            if declared.iter().any(|p| crate::app::same(p, source, &h.name)) {
                Some("spec")
            } else if h.origin == Origin::Flathub && flatpaks.contains(&h.name) {
                Some("flatpak")
            } else if matches!(h.origin, Origin::Zypper | Origin::Pacman | Origin::Apt) && distro.contains(&h.name) {
                Some("distro")
            } else {
                None
            }
        })
        .collect()
}

/// The source `fspec app install` writes for a hit, which its search id leads with except for a
/// cask.
fn source_of(h: &Hit) -> &'static str {
    match h.origin {
        Origin::Cask => "cask",
        o => o.prefix(),
    }
}

pub fn run(paths: &Paths, query: &str, json: bool, limit: usize, offline: bool) -> Result<()> {
    let fetch = LiveFetch::with_timeout(Some(INDEX));
    let cat = catalog(paths, !offline);
    if cfg!(target_os = "linux") {
        refresh_appstream(&cat, &paths.home);
    }
    let cx = fspec_sources::Cx::new(&fetch, &LiveRunner).with_catalog(cat);
    let found = catalog::search(query, &cx);
    let hits: Vec<_> = found.hits.iter().map(|(_, h)| h).take(limit).collect();
    if json {
        let (platform, distro) = &cx.local;
        let owners = owners(paths, &hits, &LiveRunner);
        for (h, owner) in hits.iter().zip(owners) {
            let source = source_of(h);
            let id = match h.origin {
                Origin::Cask => format!("cask:{}", h.name),
                _ => h.id(),
            };
            let line = json!({
                "id": id,
                "name": h.title.as_deref().unwrap_or(&h.name),
                "source": source,
                "version": h.version,
                "icon": h.icon,
                "description": h.description,
                "installed": owner.is_some(),
                "owner": owner,
                "installable": crate::app::installable(source, platform, distro),
                "run": h.origin.runs(),
                "spec": h.spec(),
            });
            println!("{line}");
        }
    } else if hits.is_empty() {
        println!("no package matches `{query}`");
    } else {
        print!("{}", catalog::table(&hits));
    }
    if catalog::VERBOSE.load(std::sync::atomic::Ordering::Relaxed) {
        for (name, e) in &found.skipped {
            eprintln!("searching {name} failed: {e}");
        }
    }
    Ok(())
}
