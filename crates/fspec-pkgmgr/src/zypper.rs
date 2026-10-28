//! What a zypper distro needs besides packages: the distro repos pinned to the Tumbleweed
//! snapshot in `fspec.lock`, and third-party `repo(...)` resources with pinned signing keys.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use fspec_core::apply::Cx;
use fspec_core::journal::{Journal, Undo, remove_any};
use fspec_core::resource::Repo;
use fspec_core::runner::Cmd;

use crate::family::zypper;

use fspec_core::zypp::{SNAPSHOT_REPOS, SNAPSHOT_VAR as VAR, snapshot_repo};

/// Rolling repos a pin disables, as paths on download.opensuse.org or cdn.opensuse.org.
/// `update` has no snapshots and would pull packages newer than the pin.
const ROLLING: [&str; 4] = ["/tumbleweed/", "/update/tumbleweed", "/debug/tumbleweed/", "/source/tumbleweed/"];

/// One section of a `/etc/zypp/repos.d/*.repo` file.
#[derive(Debug)]
pub(crate) struct RepoFile {
    pub alias: String,
    pub url: String,
    pub enabled: bool,
}

/// The repos zypper knows, read from its repo files so a rooted apply sees the rooted system.
pub(crate) fn repos(cx: &Cx) -> Vec<RepoFile> {
    let dir = cx.paths.system("/etc/zypp/repos.d");
    let Ok(entries) = fs::read_dir(&dir) else { return Vec::new() };
    let mut paths: Vec<PathBuf> =
        entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "repo")).collect();
    paths.sort();
    let mut out = Vec::new();
    for text in paths.iter().filter_map(|p| fs::read_to_string(p).ok()) {
        for line in text.lines().map(str::trim) {
            if let Some(alias) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                out.push(RepoFile { alias: alias.into(), url: String::new(), enabled: true });
            } else if let (Some(r), Some((k, v))) = (out.last_mut(), line.split_once('=')) {
                match k.trim() {
                    "baseurl" => r.url = v.trim().into(),
                    "enabled" => r.enabled = v.trim() == "1",
                    _ => {}
                }
            }
        }
    }
    out
}

fn is_rolling(url: &str) -> bool {
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")).unwrap_or(url);
    let Some((host, path)) = rest.split_once('/') else { return false };
    let path = format!("/{path}");
    matches!(host, "download.opensuse.org" | "cdn.opensuse.org") && ROLLING.iter().any(|p| path.starts_with(p))
}

/// The snapshot `fspec.lock` pins this host's distro repos to.
pub(crate) fn pin<'a>(cx: &'a Cx) -> Result<&'a str> {
    match cx.lock.zypper.get(&cx.facts.name) {
        Some(z) => Ok(&z.snapshot),
        None => bail!("fspec.lock pins no Tumbleweed snapshot for host {}; run `fspec update`", cx.facts.name),
    }
}

fn var_path(cx: &Cx) -> PathBuf {
    cx.paths.system(format!("/etc/zypp/vars.d/{VAR}"))
}

/// The snapshot the repos point at now, from the zypp variable.
pub(crate) fn live_snapshot(cx: &Cx) -> Option<String> {
    fs::read_to_string(var_path(cx)).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn add(url: &str, alias: &str) -> Cmd {
    zypper(["addrepo", "--refresh"]).arg(url).arg(alias)
}

fn remove(alias: &str) -> Cmd {
    zypper(["removerepo", alias])
}

/// Point the distro repos at `snapshot` and move every installed package to it. A rollback
/// puts the old repos back first, then runs the same upgrade against them.
pub(crate) fn move_to(cx: &Cx, journal: &mut Journal, snapshot: &str) -> Result<()> {
    let known = repos(cx);
    let dup = zypper(["dist-upgrade", "--auto-agree-with-licenses"]);
    journal.record(Undo::Run { cmd: dup.clone() })?;

    let var = var_path(cx);
    journal.backup(&var)?;
    if let Some(dir) = var.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    fs::write(&var, format!("{snapshot}\n")).with_context(|| format!("writing {}", var.display()))?;

    for (alias, path) in SNAPSHOT_REPOS {
        let file = cx.paths.system(format!("/etc/zypp/repos.d/{alias}.repo"));
        let body = snapshot_repo(alias, path);
        if fs::read_to_string(&file).is_ok_and(|t| t == body) {
            continue;
        }
        journal.backup(&file)?;
        if let Some(dir) = file.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        fs::write(&file, body).with_context(|| format!("writing {}", file.display()))?;
    }
    let rolling: Vec<&str> =
        known.iter().filter(|r| r.enabled && is_rolling(&r.url)).map(|r| r.alias.as_str()).collect();
    if !rolling.is_empty() {
        journal.record(Undo::Run { cmd: zypper(["modifyrepo", "--enable"]).args(rolling.iter().copied()) })?;
        cx.runner.check(&zypper(["modifyrepo", "--disable"]).args(rolling.iter().copied()))?;
    }
    cx.runner.check(&zypper(["refresh", "--force"]).args(SNAPSHOT_REPOS.map(|(a, _)| a)))?;
    cx.runner.check(&dup)?;
    Ok(())
}

/// Where the distro phase leaves the names it will install, for the `pkg` phase's dry run.
pub(crate) fn staged_adds(staging: &Path) -> PathBuf {
    staging.join("zypper").join("distro-adds")
}

/// Ask zypper's solver whether `names` install together with what is installed, without
/// installing them, so a conflict such as rustup against rust-src fails the apply before the
/// snapshot move and thousands of packages. Only the solver's own problems fail: a name zypper
/// cannot find yet (its `repo(...)` is added in the commit) or an unreachable repo is left to
/// the install to report.
pub(crate) fn check_solvable(cx: &Cx, names: &[String]) -> Result<()> {
    if names.is_empty() {
        return Ok(());
    }
    // `--no-refresh` reads the metadata zypper already has (the bootstrap's install left it on a
    // new host), rather than downloading the repos the snapshot move replaces anyway.
    let cmd = Cmd::new("zypper").args(["--non-interactive", "--no-refresh", "install", "--dry-run"]);
    let out = cx.runner.run(&cmd.arg("--auto-agree-with-licenses").args(names))?;
    if out.status == 0 {
        return Ok(());
    }
    let text = format!("{}\n{}", out.stdout, out.stderr);
    let mut lines = text.lines().skip_while(|l| !l.trim_start().starts_with("Problem:")).peekable();
    if lines.peek().is_none() {
        return Ok(());
    }
    let problems: Vec<&str> = lines.filter(|l| !l.trim().is_empty()).take(40).collect();
    bail!("zypper cannot install these packages together:\n{}", problems.join("\n"))
}

/// Notes for the distro repos when they are not at `snapshot`.
pub(crate) fn snapshot_drift(cx: &Cx, snapshot: &str) -> Vec<String> {
    let mut notes = Vec::new();
    match live_snapshot(cx) {
        Some(live) if live == snapshot => {}
        Some(live) => notes.push(format!("repos are at snapshot {live}, the lock says {snapshot}")),
        None => notes.push(format!("repos are not pinned, the lock says snapshot {snapshot}")),
    }
    let known = repos(cx);
    for (alias, _) in SNAPSHOT_REPOS {
        if !known.iter().any(|r| r.alias == alias && r.enabled) {
            notes.push(format!("repo {alias} is missing or disabled"));
        }
    }
    notes.extend(
        known
            .iter()
            .filter(|r| r.enabled && is_rolling(&r.url))
            .map(|r| format!("rolling repo {} is enabled", r.alias)),
    );
    notes
}

/// `repo(...)` fields zypper and rpm can use: the key and its fingerprint come together.
pub(crate) fn check_repo(r: &Repo) -> Result<()> {
    match (&r.key, &r.fingerprint) {
        (None, None) => Ok(()),
        (Some(_), Some(f)) if f.len() == 40 && f.chars().all(|c| c.is_ascii_hexdigit()) => Ok(()),
        (Some(_), Some(f)) => bail!("repo:{}: fingerprint `{f}` is not 40 hex digits", r.name),
        _ => bail!("repo:{}: key and fingerprint go together", r.name),
    }
}

fn key_path(cx: &Cx, r: &Repo) -> PathBuf {
    cx.paths.system(format!("/var/cache/fspec/keys/{}.asc", r.name))
}

/// rpm names an imported key `gpg-pubkey-<last 8 hex digits of the fingerprint>`.
fn key_package(fingerprint: &str) -> String {
    format!("gpg-pubkey-{}", fingerprint[fingerprint.len() - 8..].to_ascii_lowercase())
}

/// Download the repo's key into the cache and check its fingerprint. A rooted apply's runner
/// prints nothing, so there the check is left to the live apply. A key that cannot be fetched
/// (the host is down, or a filtering proxy intercepts TLS) leaves no key in the cache, so the
/// repo is deferred; a key with the wrong fingerprint fails the apply.
pub(crate) fn fetch_key(cx: &Cx, r: &Repo) -> Result<()> {
    let (Some(url), Some(want)) = (&r.key, &r.fingerprint) else { return Ok(()) };
    let path = key_path(cx, r);
    let bytes = match cx.fetch.get(url) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("fspec: repo:{}: fetching its key failed, deferring the repo: {e:#}", r.name);
            remove_any(&path)?;
            return Ok(());
        }
    };
    let home = cx.paths.system("/var/cache/fspec/gnupg");
    for dir in [&home, &cx.paths.system("/var/cache/fspec/keys")] {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
    let out = cx.runner.run(
        &Cmd::new("gpg")
            .args(["--batch", "--with-colons", "--show-keys"])
            .arg(path.display().to_string())
            .env("GNUPGHOME", home.display().to_string()),
    )?;
    let got: Vec<&str> = out
        .stdout
        .lines()
        .filter_map(|l| l.strip_prefix("fpr:"))
        .filter_map(|l| l.split(':').find(|f| !f.is_empty()))
        .collect();
    let rooted = cx.paths.root != std::path::Path::new("/");
    match got.first() {
        Some(f) if f.eq_ignore_ascii_case(want) => Ok(()),
        Some(f) => bail!("repo:{}: {url} has fingerprint {f}, the spec pins {want}", r.name),
        None if rooted && out.status == 0 && out.stdout.is_empty() => Ok(()),
        None => bail!("repo:{}: gpg found no key in {url}: {}", r.name, out.stderr.trim()),
    }
}

/// Import the key and add the repo, replacing a repo of the same alias at another URL. Returns
/// why the repo was deferred instead: its key could not be fetched, or its metadata cannot be
/// refreshed. A deferred repo is left disabled, so the distro upgrade and the other repos still
/// work, and the next apply tries it again.
pub(crate) fn add_repo(cx: &Cx, journal: &mut Journal, r: &Repo) -> Result<Option<String>> {
    if let Some(f) = &r.fingerprint {
        let key = key_path(cx, r);
        if !key.exists() {
            disable(cx, journal, r)?;
            return Ok(Some(format!("its key {} could not be fetched", r.key.as_deref().unwrap_or_default())));
        }
        let pkg = key_package(f);
        if cx.runner.run(&Cmd::new("rpm").args(["-q", &pkg]))?.status != 0 {
            journal.record(Undo::Run { cmd: Cmd::new("rpm").args(["-e", "--allmatches", &pkg]) })?;
            cx.runner.check(&Cmd::new("rpm").arg("--import").arg(key.display().to_string()))?;
        }
    }
    match repos(cx).into_iter().find(|k| k.alias == r.name) {
        Some(k) if k.url == r.url && k.enabled => return Ok(None),
        Some(k) => {
            journal.record(Undo::Run { cmd: add(&k.url, &k.alias) })?;
            cx.runner.check(&remove(&r.name))?;
        }
        None => {}
    }
    journal.record(Undo::Run { cmd: remove(&r.name) })?;
    cx.runner.check(&add(&r.url, &r.name))?;
    let refresh = cx.runner.run(&zypper(["refresh"]).arg(&r.name))?;
    if refresh.status != 0 {
        cx.runner.check(&zypper(["modifyrepo", "--disable"]).arg(&r.name))?;
        let why = refresh.stderr.lines().chain(refresh.stdout.lines()).find(|l| !l.trim().is_empty()).unwrap_or("");
        return Ok(Some(format!("refreshing it failed: {}", why.trim())));
    }
    Ok(None)
}

/// Turn off a repo fspec added before, so a deferred repo does not break the other repos.
fn disable(cx: &Cx, journal: &mut Journal, r: &Repo) -> Result<()> {
    if repos(cx).iter().any(|k| k.alias == r.name && k.enabled) {
        journal.record(Undo::Run { cmd: zypper(["modifyrepo", "--enable"]).arg(&r.name) })?;
        cx.runner.check(&zypper(["modifyrepo", "--disable"]).arg(&r.name))?;
    }
    Ok(())
}

/// Whether the repo is configured and enabled, so packages from it can install.
pub(crate) fn repo_enabled(cx: &Cx, alias: &str) -> bool {
    repos(cx).iter().any(|k| k.alias == alias && k.enabled)
}

/// Remove the repo. Its key stays imported, since other repos may be signed with it.
pub(crate) fn remove_repo(cx: &Cx, journal: &mut Journal, r: &Repo) -> Result<()> {
    if !repos(cx).iter().any(|k| k.alias == r.name) {
        return Ok(());
    }
    journal.record(Undo::Run { cmd: add(&r.url, &r.name) })?;
    cx.runner.check(&remove(&r.name))?;
    Ok(())
}

pub(crate) fn repo_drift(cx: &Cx, r: &Repo) -> Vec<String> {
    match repos(cx).into_iter().find(|k| k.alias == r.name) {
        None => vec![format!("repo {} is missing", r.name)],
        Some(k) if k.url != r.url => vec![format!("repo {} points at {}", r.name, k.url)],
        Some(k) if !k.enabled => vec![format!("repo {} is disabled", r.name)],
        Some(_) => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rolling_repos_are_the_stock_tumbleweed_ones() {
        assert!(is_rolling("http://download.opensuse.org/tumbleweed/repo/oss/"));
        assert!(is_rolling("http://cdn.opensuse.org/tumbleweed/repo/non-oss/"));
        assert!(is_rolling("http://download.opensuse.org/update/tumbleweed/"));
        assert!(!is_rolling("https://download.opensuse.org/history/20261020/tumbleweed/repo/oss/"));
        assert!(!is_rolling("http://codecs.opensuse.org/openh264/openSUSE_Tumbleweed"));
        assert!(!is_rolling("https://download.opensuse.org/repositories/home:x/openSUSE_Tumbleweed/"));
    }

    #[test]
    fn key_package_is_the_short_id() {
        assert_eq!(key_package("3FEF9748469ADBE15DA7CA80AC2D62742012EA22"), "gpg-pubkey-2012ea22");
    }
}
