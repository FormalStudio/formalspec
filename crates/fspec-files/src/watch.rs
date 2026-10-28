//! `fspec watch`: report drift on managed, link, patch and secret files as it happens.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use fspec_core::apply::state_dir;
use fspec_core::resource::{FileMode, Resource};
use fspec_core::{Paths, Scope, State};
use notify::{RecursiveMode, Watcher};
use serde_json::{Map, Value};

use crate::live::{Live, drift_notes, read_live};

pub struct Watched {
    pub key: String,
    pub path: PathBuf,
    pub resource: Resource,
    pub record: Map<String, Value>,
}

/// Every applied file fspec keeps in sync, from both scopes' state files.
pub fn collect(paths: &Paths) -> Result<Vec<Watched>> {
    let mut out = Vec::new();
    for scope in [Scope::System, Scope::User] {
        let state = State::load(&state_dir(paths, scope).join("state.json"))?;
        for (key, entry) in state.entries {
            let spec_path = match &entry.resource {
                Resource::File(f) if !matches!(f.mode, FileMode::Seed | FileMode::Absent) => &f.path,
                Resource::Secret(s) => &s.path,
                _ => continue,
            };
            let path = paths.resolve(spec_path)?;
            out.push(Watched { key: key.to_string(), path, resource: entry.resource, record: entry.record });
        }
    }
    Ok(out)
}

/// What to report after looking at `live`: `None` when it matches what was already reported,
/// otherwise the current findings (empty once the file is back in sync). A patch only reports
/// owned keys, so writes to keys it does not own stay quiet.
pub fn decide(w: &Watched, live: &Live, reported: &[String]) -> Option<Vec<String>> {
    let notes = drift_notes(&w.resource, &w.record, live);
    (notes != reported).then_some(notes)
}

fn announce(w: &Watched, notes: &[String], say: &mut dyn FnMut(String)) {
    if notes.is_empty() {
        say(format!("in sync {}", w.key));
    } else {
        say(format!("drift {}: {}", w.key, notes.join("; ")));
    }
}

/// Block, reporting drift through `say`, until `stop` is set.
pub fn run(paths: &Paths, say: &mut dyn FnMut(String), stop: &AtomicBool) -> Result<()> {
    let items = collect(paths)?;
    let mut reported: Vec<Vec<String>> = vec![vec![]; items.len()];
    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(tx).context("starting the file watcher")?;

    let mut by_path: BTreeMap<PathBuf, Vec<usize>> = BTreeMap::new();
    let mut dirs = BTreeSet::new();
    for (i, w) in items.iter().enumerate() {
        let (Some(parent), Some(name)) = (w.path.parent(), w.path.file_name()) else { continue };
        let Ok(parent) = parent.canonicalize() else {
            say(format!("skipping {}: {} does not exist", w.key, parent.display()));
            continue;
        };
        by_path.entry(parent.join(name)).or_default().push(i);
        dirs.insert(parent);
    }
    for dir in &dirs {
        watcher.watch(dir, RecursiveMode::NonRecursive).with_context(|| format!("watching {}", dir.display()))?;
    }

    let check = |i: usize, reported: &mut Vec<Vec<String>>, say: &mut dyn FnMut(String)| {
        let live = read_live(&items[i].path).unwrap_or(Live::Missing);
        if let Some(notes) = decide(&items[i], &live, &reported[i]) {
            announce(&items[i], &notes, say);
            reported[i] = notes;
        }
    };
    for i in 0..items.len() {
        check(i, &mut reported, say);
    }

    while !stop.load(Ordering::Relaxed) {
        let Ok(first) = rx.recv_timeout(Duration::from_millis(100)) else { continue };
        let mut hits = BTreeSet::new();
        let mut take = |ev: notify::Result<notify::Event>| {
            for path in ev.into_iter().flat_map(|e| e.paths) {
                hits.extend(by_path.get(&path).into_iter().flatten().copied());
            }
        };
        take(first);
        while let Ok(more) = rx.recv_timeout(Duration::from_millis(50)) {
            take(more);
        }
        for i in hits {
            check(i, &mut reported, say);
        }
    }
    Ok(())
}
