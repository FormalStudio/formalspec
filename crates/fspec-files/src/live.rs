//! What is on disk now, and how it compares with what the last apply recorded. Pure once the
//! live state is read, so `fspec watch` can unit-test its decisions.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fspec_core::resource::{File, FileMode, Resource};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::patch::{self, Record};
use crate::render;

pub fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Clone, Debug, PartialEq)]
pub enum Live {
    Missing,
    File(Vec<u8>),
    Link(PathBuf),
}

pub fn read_live(path: &Path) -> Result<Live> {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Live::Missing),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    if meta.file_type().is_symlink() {
        return Ok(Live::Link(fs::read_link(path)?));
    }
    Ok(Live::File(fs::read(path).with_context(|| format!("reading {}", path.display()))?))
}

pub fn recorded<'a>(record: &'a Map<String, Value>, field: &str) -> Option<&'a str> {
    record.get(field).and_then(Value::as_str)
}

/// How `live` diverged from what the last apply left, one line per finding.
pub fn drift_notes(resource: &Resource, record: &Map<String, Value>, live: &Live) -> Vec<String> {
    match resource {
        Resource::File(f) => match f.mode {
            FileMode::Managed => content_notes(record, live),
            FileMode::Seed | FileMode::Absent => vec![],
            FileMode::Link => link_notes(record, live),
            FileMode::Patch => patch_notes(f, record, live),
        },
        Resource::Secret(_) => content_notes(record, live),
        _ => vec![],
    }
}

fn content_notes(record: &Map<String, Value>, live: &Live) -> Vec<String> {
    let Some(want) = recorded(record, "sha256") else { return vec![] };
    match live {
        Live::Missing => vec!["file is missing".into()],
        Live::Link(_) => vec!["replaced by a symlink".into()],
        Live::File(bytes) if sha256(bytes) != want => vec!["content changed".into()],
        Live::File(_) => vec![],
    }
}

fn link_notes(record: &Map<String, Value>, live: &Live) -> Vec<String> {
    let Some(target) = recorded(record, "target") else { return vec![] };
    match live {
        Live::Missing => vec!["link is missing".into()],
        Live::Link(t) if t == Path::new(target) => vec![],
        Live::Link(_) => vec!["link points elsewhere".into()],
        Live::File(_) => vec!["symlink replaced by a regular file".into()],
    }
}

fn patch_notes(f: &File, record: &Map<String, Value>, live: &Live) -> Vec<String> {
    let rec = Record::from_map(record);
    let Some(format) = render::format_of(f) else { return vec![] };
    match live {
        Live::Missing if rec.keys.is_empty() => vec![],
        Live::Missing => vec!["file is missing".into()],
        Live::Link(_) => vec!["replaced by a symlink".into()],
        Live::File(bytes) => match render::parse(&String::from_utf8_lossy(bytes), format) {
            Ok(doc) => patch::drift(&doc, &f.owns, &rec),
            Err(e) => vec![format!("unreadable: {e:#}")],
        },
    }
}
