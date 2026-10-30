//! Flathub's AppStream catalog as flatpak keeps it for a remote
//! (`<installation>/appstream/flathub/<arch>/active/`): `appstream.xml.gz` and the 128px icons
//! beside it. Read into a small index, kept beside fspec's other catalogs and read again only once
//! the catalog changes, since the catalog is tens of megabytes of XML.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct App {
    pub id: String,
    pub name: Option<String>,
    pub summary: Option<String>,
    pub version: Option<String>,
    /// An absolute path to the cached icon, when the catalog ships one for the app.
    pub icon: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Index {
    /// The catalog it was read from, and that file's modification time and size.
    source: PathBuf,
    stamp: (u64, u64),
    apps: Vec<App>,
}

/// The newest of `dirs` that holds a catalog.
pub fn newest(dirs: &[PathBuf]) -> Option<PathBuf> {
    dirs.iter()
        .filter_map(|d| {
            let m = std::fs::metadata(d.join("appstream.xml.gz")).ok()?.modified().ok()?;
            Some((m, d.clone()))
        })
        .max_by_key(|(m, _)| *m)
        .map(|(_, d)| d)
}

/// The apps of the catalog in `active`, from the index at `index` when that was read from the
/// same file.
pub fn apps(active: &Path, index: &Path) -> Result<Vec<App>> {
    let source = active.join("appstream.xml.gz");
    let meta = std::fs::metadata(&source).with_context(|| format!("reading {}", source.display()))?;
    let mtime = meta.modified()?.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default();
    let stamp = (mtime, meta.len());
    if let Some(i) = std::fs::read(index).ok().and_then(|b| serde_json::from_slice::<Index>(&b).ok())
        && i.source == source
        && i.stamp == stamp
    {
        return Ok(i.apps);
    }
    let mut xml = String::new();
    flate2::read::GzDecoder::new(std::fs::File::open(&source)?)
        .read_to_string(&mut xml)
        .with_context(|| format!("reading {}", source.display()))?;
    let apps = parse(&xml, &active.join("icons/128x128"));
    if let Some(dir) = index.parent()
        && std::fs::create_dir_all(dir).is_ok()
    {
        let tmp = index.with_extension("part");
        let body = serde_json::to_vec(&Index { source, stamp, apps: apps.clone() })?;
        if std::fs::write(&tmp, body).is_ok() {
            let _ = std::fs::rename(&tmp, index);
        }
    }
    Ok(apps)
}

/// The desktop applications of an AppStream catalog. Icons are the `cached` ones, named by file in
/// `icons`, kept only when the file is there.
pub fn parse(xml: &str, icons: &Path) -> Vec<App> {
    let mut out = vec![];
    let mut rest = xml;
    // `<component ` and not `<components>`, the catalog's root.
    while let Some(start) = rest.find("<component ") {
        let after = &rest[start..];
        let Some(end) = after.find("</component>") else { break };
        let (block, next) = (&after[..end], &after[end + "</component>".len()..]);
        rest = next;
        let open = &block[..block.find('>').unwrap_or(0)];
        if !(open.contains("type=\"desktop-application\"") || open.contains("type=\"desktop\"")) {
            continue;
        }
        let Some(id) = text(block, "id") else { continue };
        let id = id.strip_suffix(".desktop").unwrap_or(&id).to_string();
        let icon = cached_icon(block).map(|f| icons.join(f)).filter(|p| p.is_file());
        out.push(App {
            name: text(block, "name"),
            summary: text(block, "summary"),
            version: attr_of(block, "<release ", "version"),
            icon: icon.map(|p| p.display().to_string()),
            id,
        });
    }
    out
}

/// The text of the first `<tag>` with no attributes, which is the untranslated one; nested
/// components (`<provides>`, `<releases>`) come after the fields read here.
fn text(block: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let start = block.find(&open)? + open.len();
    let end = block[start..].find(&format!("</{tag}>"))? + start;
    let t = unescape(block[start..end].trim());
    (!t.is_empty()).then_some(t)
}

fn cached_icon(block: &str) -> Option<&str> {
    let mut rest = block;
    let mut any = None;
    while let Some(i) = rest.find("<icon ") {
        let tail = &rest[i..];
        let close = tail.find('>')?;
        let end = tail.find("</icon>")?;
        let (attrs, file) = (&tail[..close], tail[close + 1..end].trim());
        if attrs.contains("type=\"cached\"") {
            if attrs.contains("height=\"128\"") || attrs.contains("width=\"128\"") {
                return Some(file);
            }
            any.get_or_insert(file);
        }
        rest = &tail[end..];
    }
    any
}

fn attr_of(block: &str, open: &str, key: &str) -> Option<String> {
    let tag = &block[block.find(open)?..];
    let tag = &tag[..tag.find('>')?];
    let start = tag.find(&format!(" {key}=\""))? + key.len() + 3;
    let end = tag[start..].find('"')? + start;
    Some(unescape(&tag[start..end]))
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<components version="0.8" origin="flatpak">
  <component type="desktop-application">
    <id>org.mozilla.firefox</id>
    <name>Firefox</name>
    <name xml:lang="de">Firefox-Browser</name>
    <summary>Fast, private &amp; safe web browser</summary>
    <icon type="cached" height="64" width="64">org.mozilla.firefox.png</icon>
    <icon type="cached" height="128" width="128">org.mozilla.firefox.png</icon>
    <releases>
      <release version="144.0.2" timestamp="1761609600"/>
      <release version="144.0.1" timestamp="1761004800"/>
    </releases>
  </component>
  <component type="runtime">
    <id>org.freedesktop.Platform</id>
    <name>Freedesktop Platform</name>
  </component>
  <component type="desktop">
    <id>org.gnome.Old.desktop</id>
    <name>Old</name>
  </component>
</components>
"#;

    #[test]
    fn reads_each_desktop_app_with_its_untranslated_name_newest_release_and_cached_icon() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("org.mozilla.firefox.png"), b"png").unwrap();
        let apps = parse(XML, dir.path());
        assert_eq!(apps.len(), 2, "{apps:?}");
        assert_eq!(
            apps[0],
            App {
                id: "org.mozilla.firefox".into(),
                name: Some("Firefox".into()),
                summary: Some("Fast, private & safe web browser".into()),
                version: Some("144.0.2".into()),
                icon: Some(dir.path().join("org.mozilla.firefox.png").display().to_string()),
            }
        );
        assert_eq!(apps[1].id, "org.gnome.Old");
        assert_eq!(apps[1].icon, None);
    }

    #[test]
    fn the_index_is_read_again_only_when_the_catalog_changes() {
        let dir = tempfile::tempdir().unwrap();
        let active = dir.path().join("active");
        std::fs::create_dir_all(&active).unwrap();
        let write = |xml: &str| {
            let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            std::io::Write::write_all(&mut gz, xml.as_bytes()).unwrap();
            std::fs::write(active.join("appstream.xml.gz"), gz.finish().unwrap()).unwrap();
        };
        write(XML);
        let index = dir.path().join("index.json");
        assert_eq!(apps(&active, &index).unwrap().len(), 2);
        assert!(index.is_file());
        write(&XML.replace("<id>org.gnome.Old.desktop</id>", "<id>org.gnome.NewerAndLonger</id>"));
        assert_eq!(apps(&active, &index).unwrap()[1].id, "org.gnome.NewerAndLonger");
        assert_eq!(newest(&[dir.path().join("none"), active.clone()]), Some(active));
    }
}
