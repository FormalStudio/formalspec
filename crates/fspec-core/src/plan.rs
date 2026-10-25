use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::resource::{Desired, Key, Resource, Scope};
use crate::state::State;

/// The config-level difference between a host's spec and what its state files say was applied.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Plan {
    pub host: String,
    pub changes: Vec<Change>,
    /// Resources no backend on this host can apply yet. `apply` refuses while any remain.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unsupported: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Change {
    pub key: Key,
    pub scope: Scope,
    pub action: Action,
    /// One-line description of the resource, e.g. its source or mode.
    pub detail: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    Add,
    Remove,
    /// Top-level fields whose value differs from the last-applied one.
    Update {
        fields: Vec<String>,
    },
    /// Config unchanged, but the live system no longer matches what was applied.
    Drift {
        notes: Vec<String>,
    },
    /// A removed `file` whose path another resource now writes: dropped from the state, left on
    /// disk.
    HandOver,
}

/// Fails when two declarations share a key, since only one of them could win on disk.
pub fn check_unique(desired: &[Desired]) -> Result<()> {
    let mut seen = BTreeSet::new();
    for d in desired {
        let key = d.resource.key();
        if !seen.insert(key.clone()) {
            bail!("{key} is declared more than once");
        }
    }
    Ok(())
}

pub fn plan(host: &str, desired: &[Desired], system: &State, user: &State) -> Result<Plan> {
    check_unique(desired)?;
    let mut applied = BTreeMap::new();
    for (scope, state) in [(Scope::System, system), (Scope::User, user)] {
        for (key, entry) in &state.entries {
            applied.insert(key.clone(), (scope, entry));
        }
    }

    let mut changes = Vec::new();
    for d in desired {
        let key = d.resource.key();
        let scope = d.resource.scope();
        let detail = detail(&d.resource);
        let action = match applied.remove(&key) {
            None => Action::Add,
            Some((_, entry)) => {
                let mut fields = changed_fields(&entry.resource, &d.resource);
                if entry.owner != d.owner {
                    fields.push("owner".into());
                }
                if fields.is_empty() {
                    continue;
                }
                Action::Update { fields }
            }
        };
        changes.push(Change { key, scope, action, detail });
    }
    for (key, (scope, entry)) in applied {
        changes.push(Change { key, scope, action: Action::Remove, detail: detail(&entry.resource) });
    }
    changes.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(Plan { host: host.into(), changes, unsupported: vec![] })
}

fn changed_fields(old: &Resource, new: &Resource) -> Vec<String> {
    let (Ok(Value::Object(old)), Ok(Value::Object(new))) = (serde_json::to_value(old), serde_json::to_value(new))
    else {
        return vec!["*".into()];
    };
    let keys: BTreeSet<&String> = old.keys().chain(new.keys()).collect();
    keys.into_iter().filter(|k| old.get(*k) != new.get(*k)).cloned().collect()
}

fn detail(r: &Resource) -> String {
    match r {
        Resource::Pkg(p) => p.source.to_string(),
        Resource::File(f) => format!("{:?}", f.mode).to_lowercase(),
        Resource::Service(s) => s.exec.clone().unwrap_or_else(|| "enable the distro unit".into()),
        Resource::Timer(t) => format!("{} {}", t.schedule, t.exec),
        Resource::User(u) => u.uid.map(|uid| format!("uid {uid}")).unwrap_or_default(),
        Resource::Secret(s) => s.src.clone(),
        Resource::Defaults(d) => d.value.to_string(),
        Resource::Setting(s) => s.value.to_string(),
        Resource::Image(i) => i.base.clone(),
        Resource::Distro(d) => d.distro.clone(),
        Resource::Repo(r) => r.url.clone(),
        Resource::Locale(l) => {
            [&l.locale, &l.keymap, &l.timezone].into_iter().flatten().cloned().collect::<Vec<_>>().join(" ")
        }
    }
}

impl Plan {
    pub fn counts(&self) -> (usize, usize, usize) {
        let n = |f: fn(&Action) -> bool| self.changes.iter().filter(|c| f(&c.action)).count();
        let change = n(|a| matches!(a, Action::Update { .. } | Action::Drift { .. } | Action::HandOver));
        (n(|a| matches!(a, Action::Add)), change, n(|a| matches!(a, Action::Remove)))
    }
}

impl fmt::Display for Plan {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        writeln!(f, "host {}", self.host)?;
        for c in &self.changes {
            let (sigil, extra) = match &c.action {
                Action::Add => ('+', String::new()),
                Action::Remove => ('-', String::new()),
                Action::Update { fields } => ('~', format!(" [{}]", fields.join(", "))),
                Action::Drift { notes } => ('!', format!(" drifted: {}", notes.join("; "))),
                Action::HandOver => ('>', " handed over to another resource, left on disk".into()),
            };
            let detail = if c.detail.is_empty() { String::new() } else { format!("  {}", c.detail) };
            writeln!(f, "  {sigil} {}{detail}{extra}", c.key)?;
        }
        for u in &self.unsupported {
            writeln!(f, "  ? {u}")?;
        }
        let (add, change, remove) = self.counts();
        if add + change + remove == 0 {
            write!(f, "  no changes")
        } else {
            write!(f, "  {add} to add, {change} to change, {remove} to remove")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::{File, FileMode, Pkg, Source};
    use crate::state::Entry;

    fn pkg(name: &str, source: Source) -> Resource {
        Resource::Pkg(Pkg {
            name: name.into(),
            source,
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
        })
    }

    fn file(path: &str, mode: FileMode) -> Resource {
        Resource::File(File {
            path: path.into(),
            src: Some("files/x".into()),
            target: None,
            content: None,
            format: None,
            mode,
            owns: Default::default(),
            on_conflict: Default::default(),
            on_drift: Default::default(),
            perms: None,
        })
    }

    fn state(resources: &[Resource]) -> State {
        let mut s = State::default();
        for r in resources {
            s.entries.insert(r.key(), Entry { resource: r.clone(), owner: None, record: Default::default() });
        }
        s
    }

    fn want(rs: &[Resource]) -> Vec<Desired> {
        rs.iter().map(|r| Desired { resource: r.clone(), owner: None }).collect()
    }

    #[test]
    fn add_update_remove() {
        let rg = pkg("ripgrep", Source::Aqua { name: "ripgrep".into() });
        let fd_old = pkg("fd", Source::Aqua { name: "fd".into() });
        let fd_new = pkg("fd", Source::Github { repo: "sharkdp/fd".into(), asset: None });
        let user_file = file("~/.config/x", FileMode::Managed);
        let system = state(&[fd_old, pkg("jq", Source::Aqua { name: "jq".into() })]);
        let user = state(std::slice::from_ref(&user_file));

        let p = plan("h", &want(&[rg, fd_new, user_file]), &system, &user).unwrap();
        let summary: Vec<_> = p.changes.iter().map(|c| (c.key.to_string(), c.action.clone())).collect();
        assert_eq!(
            summary,
            vec![
                ("pkg:fd".into(), Action::Update { fields: vec!["source".into()] }),
                ("pkg:jq".into(), Action::Remove),
                ("pkg:ripgrep".into(), Action::Add),
            ]
        );
        assert_eq!(p.counts(), (1, 1, 1));
    }

    #[test]
    fn duplicate_keys_fail() {
        let a = file("/etc/x", FileMode::Managed);
        let b = file("/etc/x", FileMode::Seed);
        let err = plan("h", &want(&[a, b]), &State::default(), &State::default()).unwrap_err();
        assert!(err.to_string().contains("file:/etc/x is declared more than once"));
    }
}
