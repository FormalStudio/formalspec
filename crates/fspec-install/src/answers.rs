//! An unattended install: the choices read from a JSON file instead of the screens.

use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::model::{Account, Choices, Desktop, DiskChoice, Locale, Preset, Progress, Secret, SystemChoice};
use crate::{catalog, keys};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Answers {
    pub locale: String,
    pub keymap: String,
    pub timezone: String,
    /// The whole disk to erase, `/dev/vda`.
    pub disk: String,
    #[serde(default)]
    pub passphrase: Option<String>,
    #[serde(default = "default_swap")]
    pub swap_gib: u64,
    pub distro: String,
    pub preset: String,
    /// The desktop of a desktop or gaming preset; the distro's first one when left out.
    #[serde(default)]
    pub desktop: Option<String>,
    pub user: String,
    #[serde(default)]
    pub full_name: String,
    pub hostname: String,
    pub password: String,
    /// Keys for the spec's secrets, installed into the new system. Optional.
    #[serde(default)]
    pub keys: Vec<KeyAnswer>,
    /// Make a new age key for the machine and print what to add to `secrets.star`.
    #[serde(default)]
    pub generate_key: bool,
}

/// One key: exactly one of `paste`, `file` and `ssh`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyAnswer {
    /// The key itself, `AGE-SECRET-KEY-1...` or an SSH private key.
    #[serde(default)]
    pub paste: Option<String>,
    /// A key file on a mounted disk.
    #[serde(default)]
    pub file: Option<PathBuf>,
    /// `user@host:path` on another machine.
    #[serde(default)]
    pub ssh: Option<String>,
    /// The password for `ssh`; without one, ssh uses the live session's keys.
    #[serde(default)]
    pub password: Option<String>,
}

impl KeyAnswer {
    fn source(&self) -> Result<keys::Source> {
        Ok(match (&self.paste, &self.file, &self.ssh, &self.password) {
            (Some(p), None, None, None) => keys::Source::Paste(Secret(p.clone())),
            (None, Some(f), None, None) => keys::Source::File(f.clone()),
            (None, None, Some(from), pw) => keys::Source::Ssh { from: from.clone(), password: pw.clone().map(Secret) },
            (_, _, None, Some(_)) => bail!("a key's `password` goes with `ssh` only"),
            _ => bail!("each of `keys` names exactly one of `paste`, `file` and `ssh`"),
        })
    }
}

/// The disk screen's default.
fn default_swap() -> u64 {
    4
}

const PRESETS: [Preset; 4] = [Preset::Minimal, Preset::Desktop, Preset::Gaming, Preset::Server];
const DESKTOPS: [Desktop; 3] = [Desktop::Gnome, Desktop::Kde, Desktop::Cosmic];

impl Answers {
    pub fn parse(json: &str) -> Result<Answers> {
        serde_json::from_str(json).context("reading the answers file")
    }

    /// Where each of `keys` comes from.
    pub fn key_sources(&self) -> Result<Vec<keys::Source>> {
        self.keys.iter().map(KeyAnswer::source).collect()
    }

    /// The choices the screens would have made, checked against what the catalog offers. Keys
    /// to bring are left to [`Answers::key_sources`], since reading them can reach another
    /// machine; a key to generate is in.
    pub fn choices(&self) -> Result<Choices> {
        self.key_sources()?;
        let distro =
            catalog::distro(&self.distro).with_context(|| format!("the installer offers no {}", self.distro))?;
        let preset = PRESETS.into_iter().find(|p| p.id() == self.preset);
        let preset = preset.filter(|p| distro.presets.contains(p));
        let preset = preset.with_context(|| format!("{} has no {} preset", distro.name, self.preset))?;
        let desktop = match (preset, &self.desktop) {
            (Preset::Desktop | Preset::Gaming, None) => distro.desktops.first().copied(),
            (Preset::Desktop | Preset::Gaming, Some(d)) => {
                let found = DESKTOPS.into_iter().find(|x| x.id() == d).filter(|x| distro.desktops.contains(x));
                Some(found.with_context(|| format!("{} comes without {d}", distro.name))?)
            }
            (_, Some(d)) => bail!("the {} preset has no desktop, but {d} was asked for", self.preset),
            (_, None) => None,
        };
        Ok(Choices {
            locale: Locale {
                locale: self.locale.clone(),
                keymap: self.keymap.clone(),
                timezone: self.timezone.clone(),
            },
            disk: DiskChoice {
                device: self.disk.clone(),
                encrypt: self.passphrase.clone().map(Secret),
                swap_gib: self.swap_gib,
                windows: None,
            },
            system: SystemChoice::Fresh { distro: distro.id, preset, desktop },
            account: Account {
                name: self.user.clone(),
                full_name: self.full_name.clone(),
                hostname: self.hostname.clone(),
                password: Secret(self.password.clone()),
            },
            secure_boot: false,
            keys: self.generate_key.then(keys::Key::generate).into_iter().collect(),
        })
    }
}

/// Print each stage as it starts and how long every stage took, and whether the install worked.
pub fn report(rx: Receiver<Progress>, out: &mut dyn std::io::Write) -> Result<()> {
    let start = Instant::now();
    let mut stage: Option<(String, Instant)> = None;
    let mut took: Vec<(String, Duration)> = Vec::new();
    let finish = |stage: &mut Option<(String, Instant)>, took: &mut Vec<(String, Duration)>| {
        if let Some((t, at)) = stage.take() {
            took.push((t, at.elapsed()));
        }
    };
    let mut failed = None;
    let mut skipped = Vec::new();
    for p in rx {
        match p {
            Progress::Stage { index, total, title } => {
                finish(&mut stage, &mut took);
                writeln!(out, "fspec-install: [{:>4}s] {}/{total} {title}", start.elapsed().as_secs(), index + 1)?;
                stage = Some((title, Instant::now()));
            }
            Progress::Output(line) => skipped.extend(keys::skipped_secret(&line).map(str::to_string)),
            Progress::Done => break,
            Progress::Failed { stage: s, error, log } => {
                failed = Some(format!("{s} failed: {error} (log: {})", log.display()));
                break;
            }
        }
    }
    finish(&mut stage, &mut took);
    for (t, d) in &took {
        writeln!(out, "fspec-install: {:>6}s  {t}", d.as_secs())?;
    }
    match failed {
        Some(f) => bail!(f),
        None => {
            writeln!(out, "fspec-install: done in {}s", start.elapsed().as_secs())?;
            if !skipped.is_empty() {
                writeln!(
                    out,
                    "fspec-install: left out {}, since no key here decrypts it; add a key and run `fspec apply`",
                    skipped.join(", ")
                )?;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answers(extra: &str) -> Result<Choices> {
        let json = format!(
            r#"{{"locale":"nl_BE.UTF-8","keymap":"be","timezone":"Atlantic/Canary","disk":"/dev/vda",
                "user":"kyan","hostname":"nova","password":"hunter22"{extra}}}"#
        );
        Answers::parse(&json)?.choices()
    }

    #[test]
    fn a_desktop_preset_gets_the_distros_first_desktop() {
        let c = answers(r#","distro":"arch","preset":"desktop""#).unwrap();
        assert_eq!(
            c.system,
            SystemChoice::Fresh { distro: "arch", preset: Preset::Desktop, desktop: Some(Desktop::Kde) }
        );
        assert_eq!((c.disk.swap_gib, c.disk.encrypt, c.locale.keymap.as_str()), (4, None, "be"));
        let c = answers(r#","distro":"fedora","preset":"server""#).unwrap();
        assert_eq!(c.system, SystemChoice::Fresh { distro: "fedora", preset: Preset::Server, desktop: None });
    }

    #[test]
    fn refuses_what_the_screens_would_not_offer() {
        for extra in [
            r#","distro":"gentoo","preset":"minimal""#,
            r#","distro":"aerynos","preset":"server""#,
            r#","distro":"fedora","preset":"desktop","desktop":"cosmic""#,
            r#","distro":"fedora","preset":"minimal","desktop":"gnome""#,
            r#","distro":"fedora","preset":"minimal","colour":"blue""#,
        ] {
            assert!(answers(extra).is_err(), "{extra}");
        }
    }

    #[test]
    fn keys_are_optional_and_each_names_one_source() {
        let c = answers(r#","distro":"arch","preset":"minimal""#).unwrap();
        assert!(c.keys.is_empty());
        let c = answers(r#","distro":"arch","preset":"minimal","generate_key":true"#).unwrap();
        assert!(c.keys.len() == 1 && c.keys[0].generated);
        let json = r#"{"locale":"C.UTF-8","keymap":"us","timezone":"UTC","disk":"/dev/vda","distro":"arch",
            "preset":"minimal","user":"kyan","hostname":"nova","password":"hunter22","keys":[
            {"file":"/media/usb/keys.txt"},{"ssh":"kyan@laptop:~/.config/age/keys.txt","password":"pw"}]}"#;
        let sources = Answers::parse(json).unwrap().key_sources().unwrap();
        assert_eq!(sources[0], keys::Source::File("/media/usb/keys.txt".into()));
        assert!(matches!(&sources[1], keys::Source::Ssh { password: Some(p), .. } if p.0 == "pw"));
        for bad in [
            r#","keys":[{"file":"/k","ssh":"a@b:c"}]"#,
            r#","keys":[{}]"#,
            r#","keys":[{"file":"/k","password":"pw"}]"#,
            r#","keys":[{"url":"/k"}]"#,
        ] {
            assert!(answers(&format!(r#","distro":"arch","preset":"minimal"{bad}"#)).is_err(), "{bad}");
        }
    }

    #[test]
    fn names_the_secrets_the_first_apply_left_out() {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Progress::Output("warning: skipped secret /run/wifi: no age identity found".into())).unwrap();
        tx.send(Progress::Done).unwrap();
        let mut out = Vec::new();
        report(rx, &mut out).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("left out /run/wifi, since no key here decrypts it"), "{out}");
    }

    #[test]
    fn reports_how_long_each_stage_took_and_the_failure() {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Progress::Stage { index: 0, total: 2, title: "Partitioning /dev/vda".into() }).unwrap();
        tx.send(Progress::Output("ignored".into())).unwrap();
        tx.send(Progress::Failed { stage: "Partitioning /dev/vda".into(), error: "busy".into(), log: "/l".into() })
            .unwrap();
        let mut out = Vec::new();
        let err = report(rx, &mut out).unwrap_err();
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("1/2 Partitioning /dev/vda") && out.contains("s  Partitioning /dev/vda"), "{out}");
        assert!(!out.contains("ignored"));
        assert_eq!(err.to_string(), "Partitioning /dev/vda failed: busy (log: /l)");
    }
}
