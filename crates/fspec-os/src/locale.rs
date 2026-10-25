//! A Linux host's `locale`, `keymap` and `timezone`. The files are the ones `localectl` and
//! `timedatectl` write, written directly so they work in a chroot during an install, where no
//! systemd runs. Removing the settings leaves them as they are: there is no earlier value to go
//! back to that is safer than the current one.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use fspec_core::Resource;
use fspec_core::apply::{Backend, Cx, Op, Phase, Records};
use fspec_core::facts::{Facts, Init, Os};
use fspec_core::journal::Journal;
use fspec_core::resource::Locale;
use fspec_core::runner::Cmd;
use fspec_core::state::Entry;
use serde_json::Map;

use crate::{read_or_empty, write_file};

const LOCALTIME: &str = "/etc/localtime";
const ZONEINFO: &str = "/usr/share/zoneinfo";

pub struct LocaleBackend;

/// The file that holds `LANG`.
fn lang_file(distro: &str) -> &'static str {
    match distro {
        "debian" | "ubuntu" => "/etc/default/locale",
        _ => "/etc/locale.conf",
    }
}

/// The file that holds `KEYMAP`: Void's runit reads it from `rc.conf`.
fn keymap_file(f: &Facts) -> Option<&'static str> {
    match f.init {
        Init::Systemd => Some("/etc/vconsole.conf"),
        Init::Runit if f.distro == "void" => Some("/etc/rc.conf"),
        _ => None,
    }
}

/// Locales every glibc has without generating them.
fn builtin(locale: &str) -> bool {
    matches!(locale, "C" | "POSIX" | "C.UTF-8" | "C.utf8")
}

/// The line that enables `locale` in the distro's list of locales to generate, and that list.
fn generated_in(distro: &str, locale: &str) -> Option<(&'static str, String)> {
    let charset = locale.split_once('.')?.1.split('@').next()?;
    let file = match distro {
        "arch" | "debian" => "/etc/locale.gen",
        "void" => "/etc/default/libc-locales",
        _ => return None,
    };
    Some((file, format!("{locale} {charset}")))
}

/// What builds the locale once the files are written. Ubuntu's `locale-gen` takes the locale
/// itself; Fedora ships each language's locales as a package.
pub(crate) fn generate(distro: &str, locale: &str) -> Option<Cmd> {
    if builtin(locale) {
        return None;
    }
    match distro {
        "arch" | "debian" => Some(Cmd::new("locale-gen")),
        "ubuntu" => Some(Cmd::new("locale-gen").arg(locale)),
        "void" => Some(Cmd::new("xbps-reconfigure").args(["-f", "glibc-locales"])),
        "fedora" => {
            let lang = locale.split(['_', '.', '@']).next().unwrap_or(locale);
            Some(Cmd::new("dnf").args(["install", "-y"]).arg(format!("glibc-langpack-{lang}")))
        }
        _ => None,
    }
}

/// `text` with `key=value` in place of every `key=` line, or appended when there is none.
pub(crate) fn set_var(text: &str, key: &str, value: &str) -> String {
    let want = format!("{key}={value}");
    let mut found = false;
    let mut out: Vec<String> = text
        .lines()
        .map(|l| {
            if l.trim_start().strip_prefix(key).is_some_and(|r| r.starts_with('=')) {
                found = true;
                want.clone()
            } else {
                l.to_string()
            }
        })
        .collect();
    if !found {
        out.push(want);
    }
    out.join("\n") + "\n"
}

/// `text` with `line` enabled: uncommented where the distro's list has it commented out, else
/// appended.
pub(crate) fn enable(text: &str, line: &str) -> String {
    let mut found = false;
    let mut out: Vec<String> = text
        .lines()
        .map(|l| {
            if l.trim_start_matches('#').trim() == line && !found {
                found = true;
                line.to_string()
            } else {
                l.to_string()
            }
        })
        .collect();
    if !found {
        out.push(line.to_string());
    }
    out.join("\n") + "\n"
}

fn var(text: &str, key: &str) -> Option<String> {
    text.lines().rev().find_map(|l| l.trim().strip_prefix(key)?.strip_prefix('=').map(|v| v.trim_matches('"').into()))
}

fn word(what: &str, v: &str, extra: &[char]) -> Result<()> {
    if v.is_empty() || v.starts_with(['-', '.']) || !v.chars().all(|c| c.is_ascii_alphanumeric() || extra.contains(&c))
    {
        bail!("{what} `{v}` is not a valid name");
    }
    Ok(())
}

fn zone_link(tz: &str) -> String {
    format!("..{ZONEINFO}/{tz}")
}

/// Replace `path` with `text` when it differs, backed up in the journal first.
fn edit(path: &Path, text: &str, journal: &mut Journal) -> Result<bool> {
    if read_or_empty(path).as_deref() == Some(text) {
        return Ok(false);
    }
    journal.backup(path)?;
    write_file(path, text)?;
    Ok(true)
}

impl Backend for LocaleBackend {
    fn name(&self) -> &'static str {
        "locale"
    }

    fn handles(&self, _cx: &Cx, r: &Resource) -> bool {
        matches!(r, Resource::Locale(_))
    }

    /// After the distro's packages, which bring the zoneinfo files and the locale tools.
    fn phase(&self) -> Phase {
        Phase::Defaults
    }

    fn writes(&self, cx: &Cx, r: &Resource) -> Vec<PathBuf> {
        let Resource::Locale(l) = r else { return vec![] };
        let mut paths = Vec::new();
        if l.locale.is_some() {
            paths.push(lang_file(&cx.facts.distro));
        }
        if let (Some(_), Some(f)) = (&l.keymap, keymap_file(cx.facts)) {
            paths.push(f);
        }
        if l.timezone.is_some() {
            paths.push(LOCALTIME);
        }
        paths.into_iter().map(|p| cx.paths.system(p)).collect()
    }

    fn check(&self, cx: &Cx, r: &Resource) -> Result<()> {
        let Resource::Locale(l) = r else { return Ok(()) };
        if cx.facts.os != Os::Linux {
            bail!("locale, keymap and timezone are for Linux hosts; this one is {}", cx.facts.distro);
        }
        if let Some(v) = &l.locale {
            word("locale", v, &['_', '.', '@', '-'])?;
            if !builtin(v) && !v.contains('.') {
                bail!("locale `{v}` names no character set; write it like en_US.UTF-8");
            }
        }
        if let Some(v) = &l.keymap {
            word("keymap", v, &['_', '.', '-'])?;
            if keymap_file(cx.facts).is_none() {
                bail!("keymap is set through systemd's vconsole.conf or Void's rc.conf, and this host has neither");
            }
        }
        if let Some(v) = &l.timezone {
            let ok = v.split('/').all(|part| word("timezone", part, &['_', '+', '-']).is_ok());
            if !ok {
                bail!("timezone `{v}` is not a zoneinfo name like Europe/Madrid");
            }
        }
        Ok(())
    }

    fn drift(&self, cx: &Cx, entry: &Entry) -> Result<Vec<String>> {
        let Resource::Locale(l) = &entry.resource else { return Ok(vec![]) };
        let mut notes = Vec::new();
        let read = |p: &str| read_or_empty(&cx.paths.system(p)).unwrap_or_default();
        if let Some(want) = &l.locale {
            let file = lang_file(&cx.facts.distro);
            if var(&read(file), "LANG").as_ref() != Some(want) {
                notes.push(format!("LANG in {file} is not {want}"));
            }
        }
        if let (Some(want), Some(file)) = (&l.keymap, keymap_file(cx.facts))
            && var(&read(file), "KEYMAP").as_ref() != Some(want)
        {
            notes.push(format!("KEYMAP in {file} is not {want}"));
        }
        if let Some(tz) = &l.timezone {
            let live = fs::read_link(cx.paths.system(LOCALTIME)).ok();
            if live.as_deref() != Some(Path::new(&zone_link(tz))) {
                notes.push(format!("{LOCALTIME} does not point at {tz}"));
            }
        }
        Ok(notes)
    }

    fn commit(&self, cx: &Cx, ops: &[Op], _staging: &Path, journal: &mut Journal) -> Result<Records> {
        let mut records = Records::new();
        for op in ops {
            let Resource::Locale(l) = op.resource() else { unreachable!("locale claims locales") };
            if op.is_removal() {
                continue;
            }
            apply(cx, l, journal)?;
            records.insert(op.key.clone(), Map::new());
        }
        Ok(records)
    }
}

fn apply(cx: &Cx, l: &Locale, journal: &mut Journal) -> Result<()> {
    let distro = cx.facts.distro.as_str();
    let at = |p: &str| cx.paths.system(p);
    if let Some(tz) = &l.timezone {
        if !at(&format!("{ZONEINFO}/{tz}")).is_file() {
            bail!("timezone {tz}: {ZONEINFO}/{tz} does not exist; is tzdata installed?");
        }
        let link = at(LOCALTIME);
        if fs::read_link(&link).ok().as_deref() != Some(Path::new(&zone_link(tz))) {
            journal.backup(&link)?;
            if fs::symlink_metadata(&link).is_ok() {
                fs::remove_file(&link).with_context(|| format!("removing {}", link.display()))?;
            }
            fs::create_dir_all(at("/etc"))?;
            std::os::unix::fs::symlink(zone_link(tz), &link).with_context(|| format!("linking {}", link.display()))?;
        }
        // tzdata's maintainer scripts on Debian and Ubuntu read the zone from here.
        if matches!(distro, "debian" | "ubuntu") {
            edit(&at("/etc/timezone"), &format!("{tz}\n"), journal)?;
        }
    }
    if let (Some(k), Some(file)) = (&l.keymap, keymap_file(cx.facts)) {
        let path = at(file);
        let mut text = set_var(&read_or_empty(&path).unwrap_or_default(), "KEYMAP", k);
        // systemd-localed reads the graphical layout from vconsole.conf too, and the desktops
        // take theirs from localed.
        if cx.facts.init == Init::Systemd {
            text = set_var(&text, "XKBLAYOUT", k);
        }
        edit(&path, &text, journal)?;
        // console-setup on Debian and Ubuntu reads the layout from here instead.
        if matches!(distro, "debian" | "ubuntu") {
            let path = at("/etc/default/keyboard");
            edit(&path, &set_var(&read_or_empty(&path).unwrap_or_default(), "XKBLAYOUT", k), journal)?;
        }
    }
    if let Some(locale) = &l.locale {
        let mut changed = false;
        if let Some((file, line)) = generated_in(distro, locale).filter(|_| !builtin(locale)) {
            let path = at(file);
            changed |= edit(&path, &enable(&read_or_empty(&path).unwrap_or_default(), &line), journal)?;
        }
        let path = at(lang_file(distro));
        changed |= edit(&path, &set_var(&read_or_empty(&path).unwrap_or_default(), "LANG", locale), journal)?;
        if changed && let Some(cmd) = generate(distro, locale) {
            cx.runner.check(&cmd)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
