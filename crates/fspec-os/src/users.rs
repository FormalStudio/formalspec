//! Accounts. Linux accounts are created with `useradd` before files are written, and get their
//! uid, login shell and supplementary groups with `usermod` once packages are in, since packages
//! install the shell, `usermod` itself and groups like `docker`. A Linux account without a
//! subordinate uid or gid range gets one for rootless containers. On
//! macOS the account must already exist and only the login shell is managed. Removing a `user`
//! never deletes the account.

use std::path::Path;

use anyhow::{Result, bail};
use fspec_core::Resource;
use fspec_core::apply::{Backend, Cx, Op, Phase, Records};
use fspec_core::facts::{Init, Os};
use fspec_core::journal::{Journal, Undo};
use fspec_core::resource::User;
use fspec_core::runner::Cmd;
use fspec_core::state::Entry;
use serde_json::{Map, json};

pub struct UsersBackend;

impl Backend for UsersBackend {
    fn name(&self) -> &'static str {
        "users"
    }

    fn handles(&self, _cx: &Cx, r: &Resource) -> bool {
        matches!(r, Resource::User(_))
    }

    fn phase(&self) -> Phase {
        Phase::Users
    }

    fn check(&self, cx: &Cx, r: &Resource) -> Result<()> {
        let Resource::User(u) = r else { return Ok(()) };
        check_user(u)?;
        if cx.facts.os == Os::Darwin && !u.groups.is_empty() {
            bail!("user:{} sets groups, but fspec only manages the login shell of macOS accounts", u.name);
        }
        Ok(())
    }

    fn drift(&self, cx: &Cx, entry: &Entry) -> Result<Vec<String>> {
        let Resource::User(u) = &entry.resource else { return Ok(vec![]) };
        let shell = u.shell.as_deref().map(|s| resolve_shell(cx, s));
        let mut notes = Vec::new();
        match cx.facts.os {
            Os::Linux => {
                let out = cx.runner.run(&Cmd::new("getent").args(["passwd", &u.name]))?;
                if out.status != 0 {
                    return Ok(vec![format!("account {} is missing", u.name)]);
                }
                let Some(live) = Passwd::parse(&out.stdout) else { return Ok(notes) };
                if u.uid.is_some_and(|uid| uid != live.uid) {
                    notes.push(format!("uid is {}", live.uid));
                }
                if shell.as_ref().is_some_and(|s| *s != live.shell) {
                    notes.push(format!("shell is {}", live.shell));
                }
                let groups = cx.runner.run(&Cmd::new("id").args(["-nG", &u.name]))?;
                let have: Vec<&str> = groups.stdout.split_whitespace().collect();
                if !have.is_empty() {
                    for g in u.groups.iter().filter(|g| !have.contains(&g.as_str())) {
                        notes.push(format!("not in group {g}"));
                    }
                }
                for file in SUBID_FILES {
                    if let Some(ranges) = subids(cx, file)
                        && !ranges.iter().any(|r| r.owner == u.name || r.owner == live.uid.to_string())
                    {
                        notes.push(format!("no range in {file}"));
                    }
                }
            }
            Os::Darwin => {
                if cx.runner.run(&dscl(&u.name, None))?.status != 0 {
                    return Ok(vec![format!("account {} is missing", u.name)]);
                }
                if let (Some(want), Some(live)) = (&shell, dscl_value(cx, &u.name, "UserShell")?)
                    && *want != live
                {
                    notes.push(format!("shell is {live}"));
                }
                if let (Some(want), Some(live)) = (u.uid, dscl_value(cx, &u.name, "UniqueID")?)
                    && live != want.to_string()
                {
                    notes.push(format!("uid is {live}"));
                }
            }
        }
        Ok(notes)
    }

    fn commit(&self, cx: &Cx, ops: &[Op], _staging: &Path, journal: &mut Journal) -> Result<Records> {
        let mut records = Records::new();
        for op in ops {
            let Resource::User(u) = op.resource() else { unreachable!("users claims users") };
            if op.is_removal() {
                println!("user:{} is no longer managed; the account was left in place", u.name);
                continue;
            }
            let shell = u.shell.as_deref().map(|s| resolve_shell(cx, s));
            match cx.facts.os {
                Os::Linux => create(cx, u, shell.as_deref(), journal)?,
                Os::Darwin => darwin(cx, u, shell.as_deref(), journal)?,
            }
            let mut rec = Map::new();
            if let Some(s) = shell {
                rec.insert("shell".into(), json!(s));
            }
            records.insert(op.key.clone(), rec);
        }
        Ok(records)
    }

    fn settle(&self, cx: &Cx, ops: &[Op], journal: &mut Journal) -> Result<()> {
        if cx.facts.os != Os::Linux {
            return Ok(());
        }
        let mut given = Vec::new();
        for op in ops {
            let Resource::User(u) = op.resource() else { unreachable!("users claims users") };
            if op.is_removal() {
                continue;
            }
            modify(cx, u, u.shell.as_deref().map(|s| resolve_shell(cx, s)).as_deref(), journal)?;
            add_subids(cx, u, &mut given, journal)?;
            join_groups(cx, u, journal)?;
        }
        Ok(())
    }
}

fn check_user(u: &User) -> Result<()> {
    let ok = !u.name.is_empty()
        && !u.name.starts_with('-')
        && u.name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    if !ok {
        bail!("user name `{}` may only hold letters, digits and `_ - .`, and must not start with `-`", u.name);
    }
    for g in &u.groups {
        if g.is_empty() || g.starts_with('-') || g.contains([',', ':', ' ']) {
            bail!("user:{} has the invalid group `{g}`", u.name);
        }
    }
    Ok(())
}

/// `fish` becomes the `/etc/shells` entry ending in `/fish`, else `/usr/bin/fish`.
fn resolve_shell(cx: &Cx, shell: &str) -> String {
    if shell.contains('/') {
        return shell.into();
    }
    let listed = std::fs::read_to_string(cx.paths.system("/etc/shells")).unwrap_or_default();
    listed
        .lines()
        .map(str::trim)
        .find(|l| !l.starts_with('#') && l.rsplit('/').next() == Some(shell) && l.starts_with('/'))
        .map(str::to_owned)
        .unwrap_or_else(|| format!("/usr/bin/{shell}"))
}

struct Passwd {
    uid: u32,
    shell: String,
}

impl Passwd {
    /// `name:x:uid:gid:gecos:home:shell`
    fn parse(line: &str) -> Option<Passwd> {
        let f: Vec<&str> = line.trim().split(':').collect();
        Some(Passwd { uid: f.get(2)?.parse().ok()?, shell: f.get(6)?.to_string() })
    }
}

fn passwd(cx: &Cx, name: &str) -> Result<Option<Passwd>> {
    let out = cx.runner.run(&Cmd::new("getent").args(["passwd", name]))?;
    Ok(if out.status == 0 { Passwd::parse(&out.stdout) } else { None })
}

fn create(cx: &Cx, u: &User, shell: Option<&str>, journal: &mut Journal) -> Result<()> {
    if passwd(cx, &u.name)?.is_some() {
        return Ok(());
    }
    let mut add = Cmd::new("useradd").arg("-m");
    if let Some(uid) = u.uid {
        add = add.args(["-u".to_string(), uid.to_string()]);
    }
    if let Some(shell) = shell {
        add = add.args(["-s", shell]);
    }
    journal.record(Undo::Run { cmd: Cmd::new("userdel").arg(&u.name) })?;
    cx.runner.check(&add.arg(&u.name))?;
    Ok(())
}

fn modify(cx: &Cx, u: &User, shell: Option<&str>, journal: &mut Journal) -> Result<()> {
    // No entry yet means `create` ran `useradd` in this apply, which already set the uid and shell.
    let Some(live) = passwd(cx, &u.name)? else { return Ok(()) };
    let (mut change, mut revert) = (Cmd::new("usermod"), Cmd::new("usermod"));
    let mut changed = false;
    if let Some(uid) = u.uid.filter(|uid| *uid != live.uid) {
        change = change.args(["-u".to_string(), uid.to_string()]);
        revert = revert.args(["-u".to_string(), live.uid.to_string()]);
        changed = true;
    }
    if let Some(shell) = shell.filter(|s| *s != live.shell) {
        change = change.args(["-s", shell]);
        revert = revert.args(["-s", live.shell.as_str()]);
        changed = true;
    }
    if changed {
        journal.record(Undo::Run { cmd: revert.arg(&u.name) })?;
        cx.runner.check(&change.arg(&u.name))?;
    }
    Ok(())
}

const SUBID_FILES: [&str; 2] = ["/etc/subuid", "/etc/subgid"];
/// shadow's default `SUB_UID_MIN` and `SUB_UID_COUNT`.
const SUBID_MIN: u64 = 100_000;
const SUBID_COUNT: u64 = 65_536;

struct SubRange {
    owner: String,
    start: u64,
    count: u64,
}

/// `owner:start:count` lines. `None` when the file is absent, since `usermod` then refuses
/// `--add-subuids`.
fn subids(cx: &Cx, file: &str) -> Option<Vec<SubRange>> {
    let text = std::fs::read_to_string(cx.paths.system(file)).ok()?;
    let parse = |l: &str| {
        let f: Vec<&str> = l.trim().split(':').collect();
        Some(SubRange { owner: f.first()?.to_string(), start: f.get(1)?.parse().ok()?, count: f.get(2)?.parse().ok()? })
    };
    Some(text.lines().filter_map(parse).collect())
}

/// Gives the account the first free 65536-wide block in whichever of /etc/subuid and
/// /etc/subgid lacks one, the same block in both. `given` holds blocks handed out earlier in
/// this apply, which a recording runner never writes to the files.
fn add_subids(cx: &Cx, u: &User, given: &mut Vec<u64>, journal: &mut Journal) -> Result<()> {
    let files: Vec<(&str, Vec<SubRange>)> = SUBID_FILES.iter().filter_map(|f| subids(cx, f).map(|r| (*f, r))).collect();
    let lacks = |uid: &str| -> Vec<&str> {
        let owns = |r: &SubRange| r.owner == u.name || r.owner == uid;
        files.iter().filter(|(_, ranges)| !ranges.iter().any(owns)).map(|(f, _)| *f).collect()
    };
    if lacks("").is_empty() {
        return Ok(());
    }
    let Some(live) = passwd(cx, &u.name)? else { return Ok(()) };
    let missing = lacks(&live.uid.to_string());
    if missing.is_empty() {
        return Ok(());
    }
    let taken: Vec<(u64, u64)> = files
        .iter()
        .flat_map(|(_, ranges)| ranges.iter().map(|r| (r.start, r.start + r.count)))
        .chain(given.iter().map(|s| (*s, s + SUBID_COUNT)))
        .collect();
    let mut start = SUBID_MIN;
    while let Some(&(_, end)) = taken.iter().find(|(a, b)| start < *b && *a < start + SUBID_COUNT) {
        start = SUBID_MIN + (end - SUBID_MIN).div_ceil(SUBID_COUNT) * SUBID_COUNT;
    }
    given.push(start);
    let range = format!("{start}-{}", start + SUBID_COUNT - 1);
    let (mut add, mut del) = (Cmd::new("usermod"), Cmd::new("usermod"));
    for file in missing {
        let flag = if file == "/etc/subuid" { "subuids" } else { "subgids" };
        add = add.args([format!("--add-{flag}"), range.clone()]);
        del = del.args([format!("--del-{flag}"), range.clone()]);
    }
    journal.record(Undo::Run { cmd: del.arg(&u.name) })?;
    cx.runner.check(&add.arg(&u.name))?;
    Ok(())
}

fn join_groups(cx: &Cx, u: &User, journal: &mut Journal) -> Result<()> {
    let groups = cx.runner.run(&Cmd::new("id").args(["-nG", &u.name]))?;
    let have: Vec<&str> = groups.stdout.split_whitespace().collect();
    let mut missing: Vec<&String> = u.groups.iter().filter(|g| !have.contains(&g.as_str())).collect();
    let exists = |g: &str| cx.runner.run(&Cmd::new("getent").args(["group", g])).is_ok_and(|o| o.status == 0);
    let mut absent: Vec<&String> = missing.iter().copied().filter(|g| !exists(g)).collect();
    // A group declared in /etc/sysusers.d, by a package or a `file`, exists only once
    // systemd-sysusers has run, which otherwise happens at the next boot.
    if cx.facts.init == Init::Systemd && !absent.is_empty() {
        let out = cx.runner.run(&Cmd::new("systemd-sysusers"))?;
        if out.status != 0 {
            eprintln!("fspec: systemd-sysusers exited {}: {}", out.status, out.stderr.trim());
        }
        absent.retain(|g| !exists(g));
    }
    // A deferred package has not created its group yet; drift joins it on a later apply.
    for g in &absent {
        eprintln!("fspec: user:{}: group {g} does not exist yet, joining it on a later apply", u.name);
    }
    missing.retain(|g| !absent.contains(g));
    if !missing.is_empty() {
        for g in &missing {
            journal.record(Undo::Run { cmd: Cmd::new("gpasswd").args(["-d", &u.name, g]) })?;
        }
        let list = missing.iter().map(|g| g.as_str()).collect::<Vec<_>>().join(",");
        cx.runner.check(&Cmd::new("usermod").args(["-aG", &list, &u.name]))?;
    }
    Ok(())
}

fn dscl(name: &str, key: Option<&str>) -> Cmd {
    Cmd::new("dscl").args([".", "-read"]).arg(format!("/Users/{name}")).args(key)
}

/// `dscl . -read /Users/x UserShell` prints `UserShell: /bin/zsh`.
fn dscl_value(cx: &Cx, name: &str, key: &str) -> Result<Option<String>> {
    let out = cx.runner.run(&dscl(name, Some(key)))?;
    Ok(out.stdout.trim().strip_prefix(&format!("{key}:")).map(|v| v.trim().to_string()))
}

fn darwin(cx: &Cx, u: &User, shell: Option<&str>, journal: &mut Journal) -> Result<()> {
    if cx.runner.run(&dscl(&u.name, None))?.status != 0 {
        bail!("user:{} does not exist; fspec never creates macOS accounts", u.name);
    }
    if let (Some(want), Some(live)) = (u.uid, dscl_value(cx, &u.name, "UniqueID")?)
        && live != want.to_string()
    {
        bail!("user:{} has uid {live} but the spec says {want}; fspec does not change macOS accounts", u.name);
    }
    let Some(shell) = shell else { return Ok(()) };
    let live = dscl_value(cx, &u.name, "UserShell")?;
    if live.as_deref() == Some(shell) {
        return Ok(());
    }
    if let Some(old) = live {
        journal.record(Undo::Run { cmd: Cmd::new("chsh").args(["-s", &old, &u.name]) })?;
    }
    cx.runner.check(&Cmd::new("chsh").args(["-s", shell, &u.name]))?;
    Ok(())
}

#[cfg(test)]
mod tests;
