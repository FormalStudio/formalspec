//! An unattended install: the choices read from a JSON file instead of the screens.

use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::catalog;
use crate::model::{Account, Choices, Desktop, DiskChoice, Locale, Preset, Progress, Secret, SystemChoice};

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

    /// The choices the screens would have made, checked against what the catalog offers.
    pub fn choices(&self) -> Result<Choices> {
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
    for p in rx {
        match p {
            Progress::Stage { index, total, title } => {
                finish(&mut stage, &mut took);
                writeln!(out, "fspec-install: [{:>4}s] {}/{total} {title}", start.elapsed().as_secs(), index + 1)?;
                stage = Some((title, Instant::now()));
            }
            Progress::Output(_) => {}
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
