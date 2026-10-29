//! `[zypper.<host>]`: the Tumbleweed snapshot each Tumbleweed host's distro repos point at.
//! openSUSE keeps about a month of dated snapshots under `/history/`, and `/history/list`
//! names them newest first, one `YYYYMMDD` per line.

use std::collections::BTreeMap;

use anyhow::{Context, Result, anyhow};
use fspec_core::lock::LockedZypper;
use fspec_core::resource::{Host, System};

use crate::Cx;
use crate::age::{parse_age, parse_time};

pub const LIST: &str = "https://download.opensuse.org/history/list";

fn tumbleweed(host: &Host) -> bool {
    matches!(&host.system, Some(System::Distro(d)) if d.distro == "opensuse-tumbleweed")
        && host.platform.split_once('-').is_some_and(|(arch, _)| fspec_core::zypp::pinned(arch))
}

/// When a snapshot counts as released for `release_age`: the end of its day, since the date
/// carries no time.
fn released(snapshot: &str) -> Option<i64> {
    let (y, rest) = snapshot.split_at_checked(4)?;
    let (m, d) = rest.split_at_checked(2)?;
    Some(parse_time(&format!("{y}-{m}-{d}T00:00:00Z"))? + 86_400)
}

/// The newest snapshot old enough for the host's `release_age`.
fn newest(list: &str, host: &Host, now: i64) -> Result<String> {
    let age = host.release_age.as_deref().map(parse_age).transpose()?;
    let cutoff = age.map(|a| now - a);
    let mut dated: Vec<(&str, i64)> = list
        .lines()
        .map(str::trim)
        .filter(|l| l.len() == 8 && l.bytes().all(|b| b.is_ascii_digit()))
        .filter_map(|l| Some((l, released(l)?)))
        .collect();
    dated.sort_by(|a, b| b.0.cmp(a.0));
    dated
        .into_iter()
        .find(|(_, t)| cutoff.is_none_or(|c| *t <= c))
        .map(|(s, _)| s.to_string())
        .ok_or_else(|| anyhow!("no Tumbleweed snapshot in {LIST} is old enough for the release_age policy"))
}

/// The pins for every Tumbleweed host, with one change line per moved pin. Hosts that fail
/// keep their old pin and add an error.
pub fn update(
    hosts: &[Host],
    old: &BTreeMap<String, LockedZypper>,
    cx: &Cx,
    changes: &mut Vec<String>,
    errors: &mut Vec<String>,
) -> BTreeMap<String, LockedZypper> {
    let mut pins = BTreeMap::new();
    let mut list: Option<Result<String>> = None;
    for host in hosts.iter().filter(|h| tumbleweed(h)) {
        let before = old.get(&host.name);
        let list = list.get_or_insert_with(|| {
            let bytes = cx.fetch.get(LIST).context("listing Tumbleweed snapshots")?;
            Ok(String::from_utf8_lossy(&bytes).into_owned())
        });
        let found = match list {
            Ok(text) => newest(text, host, cx.now),
            Err(e) => Err(anyhow!("{e:#}")),
        };
        match found {
            Ok(snapshot) => {
                match before {
                    None => changes.push(format!("+ snapshot {} {snapshot}", host.name)),
                    Some(b) if b.snapshot != snapshot => {
                        changes.push(format!("snapshot {} {} -> {snapshot}", host.name, b.snapshot))
                    }
                    Some(_) => {}
                }
                pins.insert(host.name.clone(), LockedZypper { snapshot });
            }
            Err(e) => {
                errors.push(format!("host {}: {e:#}", host.name));
                if let Some(b) = before {
                    pins.insert(host.name.clone(), b.clone());
                }
            }
        }
    }
    for (name, gone) in old.iter().filter(|(n, _)| !pins.contains_key(*n)) {
        changes.push(format!("- snapshot {name} {}", gone.snapshot));
    }
    pins
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(age: Option<&str>) -> Host {
        serde_json::from_value(serde_json::json!({
            "name": "e1504g",
            "platform": "x86_64-linux",
            "release_age": age,
            "system": {"kind": "distro", "distro": "opensuse-tumbleweed", "init": "systemd"},
        }))
        .unwrap()
    }

    const LIST_TEXT: &str = "20261020\n20261018\n20261017\n20261009\n";

    #[test]
    fn the_newest_snapshot_wins_without_a_release_age() {
        let now = parse_time("2026-10-21T12:00:00Z").unwrap();
        assert_eq!(newest(LIST_TEXT, &host(None), now).unwrap(), "20261020");
    }

    #[test]
    fn release_age_counts_from_the_end_of_the_snapshot_day() {
        let now = parse_time("2026-10-21T12:00:00Z").unwrap();
        assert_eq!(newest(LIST_TEXT, &host(Some("3d")), now).unwrap(), "20261017");
        assert_eq!(newest(LIST_TEXT, &host(Some("1d")), now).unwrap(), "20261018");
        assert!(newest(LIST_TEXT, &host(Some("30d")), now).is_err());
    }

    #[test]
    fn only_x86_64_hosts_are_pinned() {
        let mut arm = host(None);
        arm.platform = "aarch64-linux".into();
        assert!(tumbleweed(&host(None)));
        assert!(!tumbleweed(&arm));
    }
}
