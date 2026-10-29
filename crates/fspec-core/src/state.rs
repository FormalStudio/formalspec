use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::resource::{Key, Resource};

pub const VERSION: u32 = 1;

/// Everything FormalSpec owns in one scope. `/var/lib/fspec/state.json` for the system,
/// `~/.local/state/fspec/state.json` for the user.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct State {
    pub version: u32,
    /// Bumped by every apply that changed something; `generations/<n>.json` keeps each one.
    #[serde(default)]
    pub generation: u64,
    #[serde(default)]
    pub entries: BTreeMap<Key, Entry>,
    /// App data attributed to each package, as spec paths (`~/.config/foo`). Only the user state
    /// has any.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub data: BTreeMap<String, Vec<String>>,
    /// The entries under the XDG directories at the last user-scope apply; what appears beyond
    /// them is new. `None` until the first apply, which only takes the baseline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seen: Option<BTreeSet<String>>,
}

impl Default for State {
    fn default() -> State {
        State { version: VERSION, generation: 0, entries: BTreeMap::new(), data: BTreeMap::new(), seen: None }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Entry {
    /// The resource as last applied.
    pub resource: Resource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// What the backend did when applying it (hashes written, store path, last-applied patch keys).
    /// Each backend owns its own shape.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub record: Map<String, Value>,
}

impl State {
    /// A missing file is an empty state: nothing applied yet.
    pub fn load(path: &Path) -> Result<State> {
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(State::default()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let state: State = serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        if state.version != VERSION {
            bail!("{} is state version {}, this fspec reads {VERSION}", path.display(), state.version);
        }
        Ok(state)
    }

    /// Write-then-rename, so a crash never leaves a torn state file.
    pub fn save(&self, path: &Path) -> Result<()> {
        crate::flock::write_atomic(path, (serde_json::to_string_pretty(self)? + "\n").as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::{Pkg, Source};

    #[test]
    fn roundtrip_and_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/state.json");
        assert_eq!(State::load(&path).unwrap(), State::default());

        let pkg = Resource::Pkg(Pkg {
            name: "ripgrep".into(),
            source: Source::Aqua { name: "ripgrep".into() },
            version: None,
            links: Default::default(),
            share: Default::default(),
            parts: Default::default(),
            etc: Default::default(),
            apps: vec![],
            mode: Default::default(),
            channel: Default::default(),
            patches: vec![],
            build_flags: vec![],
            build: vec![],
            bins: vec![],
            data: vec![],
            keep_data: false,
            files: vec![],
            services: vec![],
            timers: vec![],
        });
        let mut state = State::default();
        state.entries.insert(pkg.key(), Entry { resource: pkg, owner: None, record: Map::new() });
        state.save(&path).unwrap();
        assert_eq!(State::load(&path).unwrap(), state);
        assert!(fs::read_to_string(&path).unwrap().contains("\"pkg:ripgrep\""));
    }
}
