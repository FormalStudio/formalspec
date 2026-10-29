use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use fspec_core::apply::{Backend, Cx, Op, Phase, Records};
use fspec_core::facts::Os;
use fspec_core::journal::{Journal, Undo};
use fspec_core::resource::{Distro, Repo, Resource};
use fspec_core::runner::Cmd;
use fspec_core::state::Entry;
use serde_json::Value;

use crate::family::Family;
use crate::pkgs::{drift_names, install, record_names, recorded_names, remove};
use crate::{text, zypper};

/// The `packages` list of `fedora(...)`, `arch(...)`, `tumbleweed(...)` and the other classic
/// distros, installed with that distro's package manager, and the `repo(...)` resources it
/// installs from. A name dropped from the list is removed. On Tumbleweed the distro repos
/// point at the snapshot `fspec.lock` pins.
pub struct DistroBackend;

fn distro(r: &Resource) -> Result<&Distro> {
    match r {
        Resource::Distro(d) => Ok(d),
        other => bail!("{} is not a distro", other.key()),
    }
}

fn family(cx: &Cx) -> Result<Family> {
    Family::of_distro(&cx.facts.distro)
        .with_context(|| format!("{} has no package manager FormalSpec drives", cx.facts.distro))
}

fn recorded(e: &Entry) -> Vec<String> {
    recorded_names(&e.record).unwrap_or_else(|| distro(&e.resource).map(|d| d.packages.clone()).unwrap_or_default())
}

fn snapshot_path(cx: &Cx) -> String {
    cx.paths.system(format!("/.snapshots/fspec-{}", cx.generation)).display().to_string()
}

/// How many `fspec-<generation>` snapshots an apply leaves, the newest ones.
const KEEP_SNAPSHOTS: usize = 5;

fn prune_snapshots(cx: &Cx) -> Result<()> {
    let dir = cx.paths.system("/.snapshots");
    let mut gens: Vec<u64> = fs::read_dir(&dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok()?.file_name().to_str()?.strip_prefix("fspec-")?.parse().ok())
        .collect();
    gens.sort_unstable_by(|a, b| b.cmp(a));
    for g in gens.iter().skip(KEEP_SNAPSHOTS) {
        let old = dir.join(format!("fspec-{g}")).display().to_string();
        cx.runner.check(&Cmd::new("btrfs").args(["subvolume", "delete"]).arg(old))?;
    }
    Ok(())
}

/// The record key for the Tumbleweed snapshot the last apply moved the repos to. `snapshot`
/// already names the btrfs one.
const REPO_SNAPSHOT: &str = "repo_snapshot";

/// The record key for the kernel options the last apply added to `/etc/kernel/cmdline`.
const KERNEL_ARGS: &str = "kernel_args";
const CMDLINE: &str = "/etc/kernel/cmdline";
/// Installed by `switch --beside`: it writes the boot entries on the ESP from `/etc/kernel/cmdline`.
const ESP_SYNC_UNIT: &str = "/etc/systemd/system/fspec-esp-sync.service";
/// What that unit needs beside it. Switch wrote them once; apply keeps them at this fspec's
/// version so a root an older fspec installed gets newer entry generation (Limine's kernel
/// list) and reruns the sync when `/etc/fspec/limine.conf` changes.
const ESP_SYNC_FILES: [(&str, &str); 2] = [
    ("/etc/fspec/esp-sync.sh", fspec_switch::TUMBLEWEED_ESP_SYNC),
    ("/etc/systemd/system/fspec-esp-sync.path", fspec_switch::TUMBLEWEED_ESP_SYNC_PATH),
];
/// Written by the sync from `/etc/fspec/limine.conf`, so a `file` that wrote it is handed over.
const LIMINE_CONF: &str = "/boot/efi/EFI/limine/limine.conf";

fn recorded_args(rec: &serde_json::Map<String, Value>) -> Vec<String> {
    rec.get(KERNEL_ARGS)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default()
}

/// `line` without the options the last apply added and without any option `want` sets to
/// another value (`mitigations=auto` for `mitigations=off`), then `want`.
fn merge_cmdline(line: &str, had: &[String], want: &[String]) -> String {
    let key = |o: &str| o.split_once('=').map_or(o, |(k, _)| k).to_string();
    let keys: Vec<String> = want.iter().map(|o| key(o)).collect();
    let mut out: Vec<&str> =
        line.split_whitespace().filter(|o| !had.iter().any(|h| h == o) && !keys.contains(&key(o))).collect();
    out.extend(want.iter().map(String::as_str));
    out.join(" ")
}

fn esp_sync() -> Cmd {
    Cmd::new("systemctl").args(["start", "fspec-esp-sync.service"])
}

/// Rewrite `/etc/kernel/cmdline` with `want` in place of `had`, and the boot entries from it.
fn set_kernel_args(cx: &Cx, journal: &mut Journal, had: &[String], want: &[String]) -> Result<()> {
    if had.is_empty() && want.is_empty() {
        return Ok(());
    }
    let path = cx.paths.system(CMDLINE);
    if !cx.paths.system(ESP_SYNC_UNIT).exists() {
        bail!("kernel_args need a root that `fspec switch --beside` installed; this one has no {ESP_SYNC_UNIT}");
    }
    let line = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let next = merge_cmdline(line.trim(), had, want);
    if next == line.trim() {
        return Ok(());
    }
    // Recorded first, so a rollback restores the file before it rewrites the entries.
    journal.record(Undo::Run { cmd: esp_sync() })?;
    journal.backup(&path)?;
    fs::write(&path, format!("{next}\n")).with_context(|| format!("writing {}", path.display()))?;
    cx.runner.check(&esp_sync())?;
    Ok(())
}

fn switched(cx: &Cx) -> bool {
    cx.facts.distro == "opensuse-tumbleweed" && cx.paths.system(ESP_SYNC_UNIT).exists()
}

/// The files beside fspec's ESP sync that differ from this fspec's.
fn stale_esp_sync(cx: &Cx) -> Vec<&'static str> {
    if !switched(cx) {
        return vec![];
    }
    ESP_SYNC_FILES
        .iter()
        .filter(|(path, want)| fs::read_to_string(cx.paths.system(path)).ok().as_deref() != Some(*want))
        .map(|(path, _)| *path)
        .collect()
}

/// Writes this fspec's files beside the ESP sync and runs it, so the entries and Limine's menu
/// follow at once, from a header the `file` phase may have just written.
fn refresh_esp_sync(cx: &Cx, journal: &mut Journal) -> Result<()> {
    let stale = stale_esp_sync(cx);
    if stale.is_empty() {
        return Ok(());
    }
    journal.record(Undo::Run { cmd: esp_sync() })?;
    for (path, text) in ESP_SYNC_FILES.iter().filter(|(p, _)| stale.contains(p)) {
        let path = cx.paths.system(path);
        journal.backup(&path)?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    }
    cx.runner.check(&Cmd::new("systemctl").arg("daemon-reload"))?;
    cx.runner.check(&Cmd::new("systemctl").args(["enable", "--now", "fspec-esp-sync.path"]))?;
    cx.runner.check(&esp_sync())?;
    Ok(())
}

fn repo(op: &Op) -> Option<&Repo> {
    match op.resource() {
        Resource::Repo(r) => Some(r),
        _ => None,
    }
}

/// The base packages this apply installs that the last one did not.
fn distro_adds(ops: &[Op]) -> Result<Vec<String>> {
    let mut adds = Vec::new();
    for op in ops.iter().filter(|o| repo(o).is_none()) {
        let Some(d) = &op.desired else { continue };
        let had = op.current.as_ref().map(recorded).unwrap_or_default();
        adds.extend(distro(&d.resource)?.packages.iter().filter(|n| !had.contains(n)).cloned());
    }
    Ok(adds)
}

/// Whether a distro change drops names it had.
fn drops_names(ops: &[Op]) -> Result<bool> {
    for op in ops.iter().filter(|o| repo(o).is_none()) {
        let (Some(d), Some(c)) = (&op.desired, &op.current) else { continue };
        let want = &distro(&d.resource)?.packages;
        if recorded(c).iter().any(|n| !want.contains(n) && !n.starts_with('!')) {
            return Ok(true);
        }
    }
    Ok(false)
}

impl DistroBackend {
    /// Returns why the repo was deferred, when it was.
    fn commit_repo(&self, cx: &Cx, op: &Op, journal: &mut Journal) -> Result<Option<String>> {
        match (&op.desired, &op.current) {
            (None, Some(e)) => {
                let Resource::Repo(r) = &e.resource else { return Ok(None) };
                zypper::remove_repo(cx, journal, r).map(|()| None)
            }
            (Some(d), current) => {
                let Resource::Repo(r) = &d.resource else { return Ok(None) };
                if let Some(Entry { resource: Resource::Repo(old), .. }) = current
                    && old.name != r.name
                {
                    zypper::remove_repo(cx, journal, old)?;
                }
                zypper::add_repo(cx, journal, r)
            }
            (None, None) => Ok(None),
        }
    }
}

impl Backend for DistroBackend {
    fn name(&self) -> &'static str {
        "distro"
    }

    /// Image hosts take their base packages and repos through `image(...)`.
    fn handles(&self, cx: &Cx, r: &Resource) -> bool {
        cx.facts.os == Os::Linux && !cx.facts.image && matches!(r, Resource::Distro(_) | Resource::Repo(_))
    }

    /// Before `pkg` packages, which may come from the repos and need the snapshot in place.
    fn phase(&self) -> Phase {
        Phase::Distro
    }

    fn writes(&self, cx: &Cx, r: &Resource) -> Vec<PathBuf> {
        let Resource::Distro(d) = r else { return vec![] };
        let mut paths = Vec::new();
        if !d.kernel_args.is_empty() {
            paths.push(cx.paths.system(CMDLINE));
        }
        if switched(cx) {
            paths.push(cx.paths.system(LIMINE_CONF));
        }
        paths
    }

    fn check(&self, cx: &Cx, r: &Resource) -> Result<()> {
        let fam = family(cx)?;
        match r {
            Resource::Repo(repo) if fam != Family::Zypper => {
                bail!("repo:{}: repo(...) is for zypper distros, {} is not one", repo.name, cx.facts.distro)
            }
            Resource::Repo(repo) => zypper::check_repo(repo),
            Resource::Distro(d) if fam != Family::Zypper && d.packages.iter().any(|n| n.starts_with('!')) => {
                bail!("distro: `!name` packages are zypper syntax, {} is not a zypper distro", cx.facts.distro)
            }
            Resource::Distro(d) if !d.kernel_args.is_empty() && d.distro != "opensuse-tumbleweed" => {
                bail!("distro: kernel_args are for tumbleweed(), whose boot entries fspec writes")
            }
            Resource::Distro(d) if d.kernel_args.iter().any(|a| a.is_empty() || a.contains(char::is_whitespace)) => {
                bail!("distro: each of kernel_args is one option without spaces")
            }
            _ if fam == Family::Zypper => zypper::pin(cx).map(drop),
            _ => Ok(()),
        }
    }

    fn drift(&self, cx: &Cx, entry: &Entry) -> Result<Vec<String>> {
        if let Resource::Repo(r) = &entry.resource {
            return Ok(zypper::repo_drift(cx, r));
        }
        let fam = family(cx)?;
        let mut notes = Vec::new();
        if fam == Family::Zypper
            && let Some(s) = zypper::pin(cx)?
        {
            notes.extend(zypper::snapshot_drift(cx, s));
        }
        notes.extend(drift_names(cx, fam, &recorded(entry), &entry.record)?);
        for path in stale_esp_sync(cx) {
            notes.push(format!("{path} is not this fspec's"));
        }
        let args = recorded_args(&entry.record);
        if !args.is_empty() {
            let line = fs::read_to_string(cx.paths.system(CMDLINE)).unwrap_or_default();
            let missing: Vec<_> = args.iter().filter(|a| !line.split_whitespace().any(|o| o == *a)).cloned().collect();
            if !missing.is_empty() {
                notes.push(format!("not in {CMDLINE}: {}", missing.join(" ")));
            }
        }
        Ok(notes)
    }

    /// Takes the btrfs snapshot here because every backend prepares before any commits, so the
    /// snapshot holds the system from before the `pkg` packages and the new kernel. Repo keys
    /// are fetched and checked here too, before anything changes.
    fn prepare(&self, cx: &Cx, ops: &[Op], staging: &Path) -> Result<()> {
        for op in ops.iter().filter(|o| o.desired.is_some()) {
            if let Some(r) = repo(op) {
                zypper::fetch_key(cx, r)?;
            }
        }
        if ops.iter().all(Op::is_removal) {
            return Ok(());
        }
        // A dropped name can be what an add conflicts with (rust-src under rustup), and its removal
        // commits before the adds, which one dry run cannot model. The install reports it then.
        if family(cx)? == Family::Zypper && !drops_names(ops)? {
            let adds = distro_adds(ops)?;
            let file = zypper::staged_adds(staging);
            fs::create_dir_all(file.parent().context("the file has a parent")?)?;
            fs::write(&file, adds.join("\n")).with_context(|| format!("writing {}", file.display()))?;
            zypper::check_solvable(cx, &adds)?;
        }
        let fstype = cx
            .runner
            .run(&Cmd::new("findmnt").args(["-no", "FSTYPE"]).arg(cx.paths.system("/").display().to_string()))?;
        if fstype.stdout.trim() != "btrfs" {
            return Ok(());
        }
        let dest = snapshot_path(cx);
        if Path::new(&dest).exists() {
            return Ok(());
        }
        if let Some(dir) = Path::new(&dest).parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let root = cx.paths.system("/").display().to_string();
        cx.runner.check(&Cmd::new("btrfs").args(["subvolume", "snapshot", "-r"]).arg(root).arg(&dest))?;
        prune_snapshots(cx)
    }

    /// Repos first, then the snapshot, then the base packages, which may come from either.
    fn commit(&self, cx: &Cx, ops: &[Op], _staging: &Path, journal: &mut Journal) -> Result<Records> {
        let fam = family(cx)?;
        let mut records = Records::new();
        for op in ops.iter().filter(|o| repo(o).is_some()) {
            if let Some(why) = self.commit_repo(cx, op, journal)? {
                records.insert(op.key.clone(), fspec_core::apply::deferred(why));
            }
        }
        for op in ops.iter().filter(|o| repo(o).is_none()) {
            let Some(d) = &op.desired else { continue };
            let want = &distro(&d.resource)?.packages;
            let had = op.current.as_ref().map(recorded).unwrap_or_default();
            let rec = op.record();
            let pinned = match fam {
                Family::Zypper => zypper::pin(cx)?,
                _ => None,
            };
            if let Some(s) = pinned
                && (text(&rec, REPO_SNAPSHOT).as_deref() != Some(s) || !zypper::snapshot_drift(cx, s).is_empty())
            {
                zypper::move_to(cx, journal, s)?;
            }
            let dropped: Vec<String> =
                had.iter().filter(|n| !want.contains(n) && !n.starts_with('!')).cloned().collect();
            let added: Vec<String> = want.iter().filter(|n| !had.contains(n)).cloned().collect();
            if !dropped.is_empty() {
                remove(cx, journal, fam, &dropped, &rec)?;
            }
            if !added.is_empty() {
                install(cx, fam, &added, None)?;
            }
            refresh_esp_sync(cx, journal)?;
            let args = &distro(&d.resource)?.kernel_args;
            set_kernel_args(cx, journal, &recorded_args(&rec), args)?;
            let mut next = record_names(cx, fam, want)?;
            if !args.is_empty() {
                next.insert(KERNEL_ARGS.into(), Value::from(args.clone()));
            }
            let snap = snapshot_path(cx);
            if Path::new(&snap).exists() {
                next.insert("snapshot".into(), Value::from(snap));
            }
            if let Some(s) = pinned {
                next.insert(REPO_SNAPSHOT.into(), Value::from(s));
            }
            records.insert(op.key.clone(), next);
        }
        for op in ops.iter().filter(|o| o.desired.is_some() && repo(o).is_some()) {
            records.entry(op.key.clone()).or_default();
        }
        Ok(records)
    }
}
