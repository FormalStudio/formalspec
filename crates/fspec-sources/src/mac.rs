//! `mac(...)`: a Mac app's DMG or archive, from a Sparkle appcast or a `github(...)` release,
//! locked once for apply to hand to atoll.

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use fspec_core::lock::LockedPkg;

use crate::Cx;
use crate::age::parse_rfc2822;
use crate::github::{Platform, match_asset, pick_release, trim_v};
use crate::hash::sri;

/// One release in an appcast: the `<item>` and its main `<enclosure>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// `sparkle:version`, the bundle's `CFBundleVersion`, which Sparkle compares.
    pub version: String,
    /// `sparkle:shortVersionString`, the version the app shows.
    pub short: Option<String>,
    pub url: String,
    pub ed_signature: Option<String>,
    pub published: Option<i64>,
}

impl Item {
    /// The lock's version: what the app shows, plus the build when that differs, since a preview
    /// feed ships many builds of one `0.1.0`.
    pub fn lock_version(&self) -> String {
        match &self.short {
            Some(s) if *s != self.version => format!("{s}+{}", self.version),
            _ => self.version.clone(),
        }
    }
}

/// The releases a Sparkle updater with no channel would see: items on a `sparkle:channel` and
/// enclosures for another `sparkle:os` are left out, as are delta updates.
pub fn parse(xml: &str) -> Vec<Item> {
    let mut out = vec![];
    for chunk in xml.split("<item").skip(1) {
        if !chunk.starts_with(['>', ' ', '\n', '\r', '\t']) {
            continue;
        }
        let Some(end) = chunk.find("</item>") else { continue };
        let block = without(&chunk[..end], "<sparkle:deltas>", "</sparkle:deltas>");
        if block.contains("<sparkle:channel>") {
            continue;
        }
        let Some(enclosure) = tag(&block, "<enclosure") else { continue };
        if attr(enclosure, "sparkle:os").is_some_and(|os| os != "macos") {
            continue;
        }
        let (Some(url), Some(version)) =
            (attr(enclosure, "url"), text(&block, "sparkle:version").or_else(|| attr(enclosure, "sparkle:version")))
        else {
            continue;
        };
        out.push(Item {
            short: text(&block, "sparkle:shortVersionString").or_else(|| attr(enclosure, "sparkle:shortVersionString")),
            ed_signature: attr(enclosure, "sparkle:edSignature"),
            published: text(&block, "pubDate").as_deref().and_then(parse_rfc2822),
            url,
            version,
        });
    }
    out
}

/// The highest `sparkle:version`, among those published before `cutoff` when one is set.
pub fn newest(items: Vec<Item>, cutoff: Option<i64>) -> Option<Item> {
    items
        .into_iter()
        .filter(|i| cutoff.is_none_or(|c| i.published.is_some_and(|t| t <= c)))
        .max_by(|a, b| compare(&a.version, &b.version))
}

/// Dotted versions part by part, numbers as numbers, as Sparkle's standard comparator orders
/// `1.10` after `1.9`.
fn compare(a: &str, b: &str) -> std::cmp::Ordering {
    let parts = |v: &str| v.split('.').map(str::to_string).collect::<Vec<_>>();
    let (a, b) = (parts(a), parts(b));
    for (x, y) in a.iter().zip(&b) {
        let o = match (x.parse::<u64>(), y.parse::<u64>()) {
            (Ok(x), Ok(y)) => x.cmp(&y),
            _ => x.cmp(y),
        };
        if o.is_ne() {
            return o;
        }
    }
    a.len().cmp(&b.len())
}

/// Sparkle signs the archive's bytes with Ed25519; the app carries the public key as
/// `SUPublicEDKey`.
pub fn check_signature(key: &str, item: &Item, archive: &[u8]) -> Result<()> {
    let sig = item.ed_signature.as_deref().with_context(|| format!("{} has no sparkle:edSignature", item.url))?;
    let key = STANDARD.decode(key.trim()).map_err(|e| anyhow!("ed_key is not base64: {e}"))?;
    let sig = STANDARD
        .decode(sig.trim())
        .map_err(|e| anyhow!("the sparkle:edSignature of {} is not base64: {e}", item.url))?;
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &key)
        .verify(archive, &sig)
        .map_err(|_| anyhow!("{} does not match its sparkle:edSignature under ed_key", item.url))
}

pub fn appcast(feed: &str, ed_key: Option<&str>, cutoff: Option<i64>, cx: &Cx, out: &mut LockedPkg) -> Result<()> {
    let xml = String::from_utf8(cx.fetch.get(feed)?).with_context(|| format!("{feed} is not UTF-8"))?;
    let items = parse(&xml);
    if items.is_empty() {
        bail!("{feed} lists no release with an enclosure");
    }
    let item = newest(items, cutoff)
        .with_context(|| format!("no release in {feed} is old enough for the release_age policy"))?;
    let archive = cx.fetch.get(&item.url)?;
    if let Some(key) = ed_key {
        check_signature(key, &item, &archive)?;
    }
    out.version = Some(item.lock_version());
    out.artifacts.insert(
        "any".into(),
        fspec_core::lock::Artifact { url: Some(item.url), hash: Some(sri(&archive)), ..Default::default() },
    );
    Ok(())
}

/// One asset per Linux platform, matched as the Mac build of the same CPU: atoll runs the slice
/// of the machine's own architecture, so a universal DMG serves both.
pub fn github(
    repo: &str,
    glob: Option<&str>,
    platforms: &[String],
    cutoff: Option<i64>,
    cx: &Cx,
    out: &mut LockedPkg,
) -> Result<()> {
    let release = pick_release(repo, cutoff, None, cx)?;
    out.version = Some(trim_v(&release.tag_name).to_string());
    for p in platforms {
        let darwin = format!("{}-darwin", p.split('-').next().unwrap_or_default());
        let asset = match_asset(&release.assets, glob, Platform::parse(&darwin)?)
            .with_context(|| format!("{repo} {} for {darwin}", release.tag_name))?;
        out.artifacts.insert(p.clone(), crate::github::artifact(asset, cx)?);
    }
    Ok(())
}

fn without(block: &str, open: &str, close: &str) -> String {
    let mut out = String::new();
    let mut rest = block;
    while let Some(i) = rest.find(open) {
        out.push_str(&rest[..i]);
        rest = rest[i..].find(close).map_or("", |e| &rest[i + e + close.len()..]);
    }
    out.push_str(rest);
    out
}

/// The first `<name ...>` tag of `block`, up to its `>`.
fn tag<'a>(block: &'a str, open: &str) -> Option<&'a str> {
    let at = block.find(&format!("{open} ")).or_else(|| block.find(&format!("{open}\n")))?;
    let tag = &block[at..];
    Some(&tag[..tag.find('>')?])
}

fn attr(tag: &str, key: &str) -> Option<String> {
    for quote in ['"', '\''] {
        for lead in [' ', '\n', '\t', '\r'] {
            let open = format!("{lead}{key}={quote}");
            if let Some(start) = tag.find(&open).map(|i| i + open.len()) {
                let end = tag[start..].find(quote)? + start;
                return Some(unescape(&tag[start..end]));
            }
        }
    }
    None
}

fn text(block: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let start = block.find(&open)? + open.len();
    let end = block[start..].find(&format!("</{name}>"))? + start;
    let t = unescape(block[start..end].trim());
    (!t.is_empty()).then_some(t)
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;
    use fspec_core::fetch::MapFetch;
    use fspec_core::runner::RecordingRunner;
    use ring::signature::{Ed25519KeyPair, KeyPair};

    const FEED: &str = r#"<?xml version="1.0" standalone="yes"?>
<rss xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle" version="2.0">
    <channel>
        <title>Rex</title>
        <item>
            <title>0.1.0</title>
            <pubDate>Fri, 06 Nov 2026 18:05:29 +0000</pubDate>
            <sparkle:version>1853</sparkle:version>
            <sparkle:shortVersionString>0.1.0</sparkle:shortVersionString>
            <enclosure url="https://example.com/Rex-0.1.0-1853.dmg" length="5" type="application/octet-stream" sparkle:edSignature="SIG1853"/>
        </item>
        <item>
            <title>0.1.0</title>
            <pubDate>Tue, 10 Nov 2026 16:09:07 +0000</pubDate>
            <sparkle:version>1925</sparkle:version>
            <sparkle:shortVersionString>0.1.0</sparkle:shortVersionString>
            <sparkle:deltas>
                <enclosure url="https://example.com/Rex1925-1853.delta" sparkle:deltaFrom="1853" sparkle:edSignature="DELTA"/>
            </sparkle:deltas>
            <enclosure url="https://example.com/Rex-0.1.0-1925.dmg?a=1&amp;b=2" length="5" type="application/octet-stream" sparkle:edSignature="SIG1925"/>
        </item>
        <item>
            <title>0.2.0 beta</title>
            <sparkle:channel>beta</sparkle:channel>
            <sparkle:version>2000</sparkle:version>
            <enclosure url="https://example.com/Rex-beta.dmg"/>
        </item>
        <item>
            <title>Windows</title>
            <enclosure url="https://example.com/Rex.msi" sparkle:version="3000" sparkle:os="windows"/>
        </item>
        <item>
            <title>Old style</title>
            <pubDate>Mon, 02 Nov 2026 09:00:00 GMT</pubDate>
            <enclosure url='https://example.com/Rex-0.0.9.zip' sparkle:version='999' sparkle:shortVersionString='0.0.9'/>
        </item>
    </channel>
</rss>
"#;

    #[test]
    fn an_appcast_yields_the_releases_a_channelless_updater_sees() {
        let items = parse(FEED);
        let urls: Vec<_> = items.iter().map(|i| i.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "https://example.com/Rex-0.1.0-1853.dmg",
                "https://example.com/Rex-0.1.0-1925.dmg?a=1&b=2",
                "https://example.com/Rex-0.0.9.zip"
            ]
        );
        assert_eq!(items[1].ed_signature.as_deref(), Some("SIG1925"));
        assert_eq!(items[2].short.as_deref(), Some("0.0.9"));
        assert_eq!(items[2].version, "999");
        assert_eq!(items[0].published, parse_rfc2822("Fri, 06 Nov 2026 18:05:29 +0000"));
    }

    #[test]
    fn the_newest_is_the_highest_build_and_release_age_holds_it_back() {
        let newest_at = |cutoff| newest(parse(FEED), cutoff).map(|i| i.lock_version());
        assert_eq!(newest_at(None).as_deref(), Some("0.1.0+1925"));
        assert_eq!(newest_at(parse_rfc2822("Sat, 07 Nov 2026 00:00:00 +0000")).as_deref(), Some("0.1.0+1853"));
        assert_eq!(newest_at(parse_rfc2822("Sun, 01 Nov 2026 00:00:00 +0000")), None);
        assert_eq!(compare("1.10", "1.9"), std::cmp::Ordering::Greater);
        assert_eq!(compare("2", "2.0.1"), std::cmp::Ordering::Less);
    }

    fn signed(archive: &[u8]) -> (String, String) {
        let rng = ring::rand::SystemRandom::new();
        let pair = Ed25519KeyPair::from_pkcs8(Ed25519KeyPair::generate_pkcs8(&rng).unwrap().as_ref()).unwrap();
        (STANDARD.encode(pair.public_key().as_ref()), STANDARD.encode(pair.sign(archive).as_ref()))
    }

    #[test]
    fn update_locks_the_newest_enclosure_once_its_signature_checks_out() {
        let (key, sig) = signed(b"rex-1925");
        let feed = FEED.replace("SIG1925", &sig);
        let fetch = MapFetch::default()
            .with("https://example.com/appcast.xml", feed)
            .with("https://example.com/Rex-0.1.0-1925.dmg?a=1&b=2", "rex-1925");
        let runner = RecordingRunner::default();
        let cx = Cx::new(&fetch, &runner);
        let mut out = LockedPkg {
            name: "rex".into(),
            source: "mac:appcast:https://example.com/appcast.xml".into(),
            version: None,
            rev: None,
            patches: None,
            parts: Default::default(),
            artifacts: Default::default(),
        };
        appcast("https://example.com/appcast.xml", Some(&key), None, &cx, &mut out).unwrap();
        assert_eq!(out.version.as_deref(), Some("0.1.0+1925"));
        let any = &out.artifacts["any"];
        assert_eq!(any.url.as_deref(), Some("https://example.com/Rex-0.1.0-1925.dmg?a=1&b=2"));
        assert_eq!(any.hash.as_deref(), Some(sri(b"rex-1925").as_str()));

        let (other, _) = signed(b"rex-1925");
        let err = appcast("https://example.com/appcast.xml", Some(&other), None, &cx, &mut out).unwrap_err();
        assert!(err.to_string().contains("does not match its sparkle:edSignature"), "{err}");
    }

    #[test]
    fn a_release_dmg_locks_for_each_linux_cpu_as_the_mac_build_of_that_cpu() {
        let release = r#"{"tag_name": "5.1.0", "assets": [
            {"name": "CotEditor_5.1.0.dmg", "browser_download_url": "https://github.com/coteditor/CotEditor/releases/download/5.1.0/CotEditor_5.1.0.dmg",
             "digest": "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"},
            {"name": "CotEditor_5.1.0.dmg.sig", "browser_download_url": "https://example.com/sig"}
        ]}"#;
        let fetch =
            MapFetch::default().with("https://api.github.com/repos/coteditor/CotEditor/releases/latest", release);
        let runner = RecordingRunner::default();
        let cx = Cx::new(&fetch, &runner);
        let mut out = LockedPkg {
            name: "coteditor".into(),
            source: "mac:github:coteditor/CotEditor".into(),
            version: None,
            rev: None,
            patches: None,
            parts: Default::default(),
            artifacts: Default::default(),
        };
        let platforms = ["x86_64-linux".to_string(), "aarch64-linux".to_string()];
        github("coteditor/CotEditor", Some("*.dmg"), &platforms, None, &cx, &mut out).unwrap();
        assert_eq!(out.version.as_deref(), Some("5.1.0"));
        for p in &platforms {
            let a = &out.artifacts[p];
            assert!(a.url.as_deref().unwrap().ends_with("/CotEditor_5.1.0.dmg"), "{p}");
            assert_eq!(a.hash.as_deref(), Some("sha256-LPJNul+wow4m6DsqxbninhsWHlwfp0JecwQzYpOLmCQ="));
        }
    }
}
