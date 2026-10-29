use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// `fspec.lock`: every version FormalSpec installs, written by `fspec update` and committed.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Lock {
    #[serde(default)]
    pub pkg: Vec<LockedPkg>,
    /// Per host: the system layer's pinned digest.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub system: BTreeMap<String, LockedSystem>,
    /// Per Tumbleweed host: the snapshot its distro repos point at.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub zypper: BTreeMap<String, LockedZypper>,
    /// For a project's `dev.star`: its `lib(...)` and `bin(...)` dependencies, per package manager.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub native: Vec<LockedNative>,
    /// For a project's `dev.star`: each OCI image it names (`debian:13`) pinned by digest.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub image: BTreeMap<String, String>,
    /// Each `load("gh:owner/repo@ref//...")` source, keyed `gh:owner/repo@ref` (no `@ref` for the
    /// default branch): the commit it resolved to and the files loaded from it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub loads: BTreeMap<String, LockedLoad>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct LockedLoad {
    pub rev: String,
    /// Path inside the repo to its `sha256-<base64>`.
    pub files: BTreeMap<String, String>,
}

/// Where a spec keeps the load files a remote apply shipped with it, relative to the spec dir.
pub const SHIPPED_LOADS: &str = ".fspec/loads";

/// `<owner>/<repo>/<rev>/<path>`, the layout of the load cache and of [`SHIPPED_LOADS`].
pub fn load_rel(repo: &str, rev: &str, path: &str) -> PathBuf {
    Path::new(repo).join(rev).join(path)
}

/// `$XDG_CACHE_HOME/fspec`, else `~/.cache/fspec`.
pub fn cache_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("fspec"))
}

/// `$XDG_CACHE_HOME/fspec/loads`, else `~/.cache/fspec/loads`.
pub fn loads_cache() -> Option<PathBuf> {
    Some(cache_dir()?.join("loads"))
}

impl LockedLoad {
    /// Each locked file under the load cache layout. `key` is the lock key, `gh:owner/repo@ref`.
    pub fn rel_paths(&self, key: &str) -> Vec<PathBuf> {
        let repo = key.trim_start_matches("gh:").split('@').next().unwrap_or_default();
        self.files.keys().map(|path| load_rel(repo, &self.rev, path)).collect()
    }
}

/// What provides one `lib(...)` or `bin(...)`: the package (or `pkgconfig(...)`, `/usr/bin/...`
/// capability) to install, keyed by package manager, and why a lookup found nothing.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct LockedNative {
    /// `lib` or `bin`.
    pub kind: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub miss: BTreeMap<String, String>,
    #[serde(flatten)]
    pub provides: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct LockedPkg {
    pub name: String,
    /// `Source`'s display form, e.g. `aqua:BurntSushi/ripgrep`. A spec whose source no longer
    /// matches needs `fspec update`.
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    /// Hash over the package's patch files, for patched source builds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patches: Option<String>,
    /// `pkg(parts = ...)`, keyed by the path each unpacks into.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub parts: BTreeMap<String, Artifact>,
    /// Keyed by platform (`x86_64-linux`, `aarch64-darwin`), or `any` for platform-independent
    /// artifacts such as npm tarballs.
    #[serde(flatten)]
    pub artifacts: BTreeMap<String, Artifact>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Artifact {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// `sha256-<base64>`, SRI style.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    /// `registry/repo@sha256:…` for CI-built artifacts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oci: Option<String>,
    /// The build key of a CI-built `git(...)` artifact: eight hex digits over its commit, patches
    /// and recipe. A host installs `oci` only while its own key for the package is the same.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
    /// Flatpak commit, git rev, or another source-specific exact id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Executables inside the unpacked artifact, relative to its root (`rg`,
    /// `ripgrep-15.1.0-x86_64-unknown-linux-musl/rg`). Empty: link what a prefix tree's `bin/`
    /// holds, else every executable regular file in the top two levels. `<command>=<path>` names
    /// the command differently.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bins: Vec<String>,
    /// For pypi packages: every distribution to install, one `name==version --hash=sha256:...`
    /// line each, as uv compiles them for all platforms.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requirements: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct LockedSystem {
    pub image: String,
}

/// A dated openSUSE Tumbleweed snapshot (`20261020`), served under
/// `https://download.opensuse.org/history/<snapshot>/tumbleweed/`.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct LockedZypper {
    pub snapshot: String,
}

impl Lock {
    /// A missing lock is empty; `plan` then reports packages that need `fspec update`.
    pub fn load(path: &Path) -> Result<Lock> {
        match fs::read_to_string(path) {
            Ok(t) => toml::from_str(&t).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Lock::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        crate::flock::write_atomic(path, toml::to_string(self)?.as_bytes())
    }

    /// Save what this run changed from `base`, the lock it read, onto the lock as it is on disk
    /// now. Two `fspec update`s of different packages at once then both land: each re-reads the
    /// file under a lock and moves only its own entries.
    pub fn save_over(&self, base: &Lock, path: &Path) -> Result<()> {
        let name = std::path::absolute(path)?.display().to_string();
        let guard = cache_dir().unwrap_or_else(std::env::temp_dir).join("locks").join(format!("{}.lock", hash8(&name)));
        let _held = crate::flock::acquire(&guard, &format!("write of {}", path.display()), true)?;
        let mut disk = Lock::load(path)?;
        disk.take_changes(base, self);
        disk.save(path)
    }

    /// Apply every entry that differs between `base` and `new` to `self`: replaced or added
    /// where `new` has it, removed where it does not.
    fn take_changes(&mut self, base: &Lock, new: &Lock) {
        let pkg_key = |p: &LockedPkg| (p.name.clone(), p.source.clone());
        for key in base.pkg.iter().chain(&new.pkg).map(pkg_key).collect::<std::collections::BTreeSet<_>>() {
            let find = |l: &Lock| l.pkg(&key.0, &key.1).cloned();
            match (find(base), find(new)) {
                (was, now) if was == now => {}
                (_, Some(now)) => self.upsert(now),
                (_, None) => self.pkg.retain(|p| pkg_key(p) != key),
            }
        }
        let native_key = |n: &LockedNative| (n.kind.clone(), n.name.clone());
        for key in base.native.iter().chain(&new.native).map(native_key).collect::<std::collections::BTreeSet<_>>() {
            let find = |l: &Lock| l.native(&key.0, &key.1).cloned();
            match (find(base), find(new)) {
                (was, now) if was == now => {}
                (_, Some(now)) => match self.native.iter_mut().find(|n| native_key(n) == key) {
                    Some(slot) => *slot = now,
                    None => self.native.push(now),
                },
                (_, None) => self.native.retain(|n| native_key(n) != key),
            }
        }
        take_map(&mut self.system, &base.system, &new.system);
        take_map(&mut self.zypper, &base.zypper, &new.zypper);
        take_map(&mut self.image, &base.image, &new.image);
        take_map(&mut self.loads, &base.loads, &new.loads);
    }

    pub fn native(&self, kind: &str, name: &str) -> Option<&LockedNative> {
        self.native.iter().find(|n| n.kind == kind && n.name == name)
    }

    /// The entry for `name` from `source` (`Source`'s display form). Hosts can take one package
    /// name from different sources (brew on the macbook, github on Linux), so both are the key.
    pub fn pkg(&self, name: &str, source: &str) -> Option<&LockedPkg> {
        self.pkg.iter().find(|p| p.name == name && p.source == source)
    }

    /// Every entry named `name`, whatever its source.
    pub fn named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a LockedPkg> {
        self.pkg.iter().filter(move |p| p.name == name)
    }

    /// Insert or replace by name and source, keeping the file sorted so diffs stay small.
    pub fn upsert(&mut self, locked: LockedPkg) {
        self.pkg.retain(|p| p.name != locked.name || p.source != locked.source);
        self.pkg.push(locked);
        self.pkg.sort_by(|a, b| (&a.name, &a.source).cmp(&(&b.name, &b.source)));
    }
}

fn take_map<V: Clone + PartialEq>(
    into: &mut BTreeMap<String, V>,
    base: &BTreeMap<String, V>,
    new: &BTreeMap<String, V>,
) {
    for key in base.keys().chain(new.keys()).cloned().collect::<std::collections::BTreeSet<_>>() {
        match (base.get(&key), new.get(&key)) {
            (was, now) if was == now => {}
            (_, Some(now)) => {
                into.insert(key, now.clone());
            }
            (_, None) => {
                into.remove(&key);
            }
        }
    }
}

fn hash8(s: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(s.as_bytes()).iter().take(4).map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn design_example_parses() {
        let text = r#"
[[pkg]]
name = "ripgrep"
source = "aqua:BurntSushi/ripgrep"
version = "15.1.0"
x86_64-linux = { url = "https://example/rg-x86.tar.gz", hash = "sha256-AAAA" }
aarch64-darwin = { url = "https://example/rg-arm.tar.gz", hash = "sha256-BBBB" }

[[pkg]]
name = "librepods"
source = "git:https://example/librepods@main"
rev = "4f1c2a9"
patches = "sha256-CCCC"
x86_64-linux = { oci = "ghcr.io/formalstudio/fspec-cache/librepods@sha256:dd" }

[system.g815]
image = "ghcr.io/ublue-os/bazzite-nvidia@sha256:ee"

[zypper.e1504g]
snapshot = "20261020"
"#;
        let lock: Lock = toml::from_str(text).unwrap();
        assert_eq!(
            lock.named("ripgrep").next().unwrap().artifacts["aarch64-darwin"].hash.as_deref(),
            Some("sha256-BBBB")
        );
        assert!(lock.named("librepods").next().unwrap().artifacts["x86_64-linux"].oci.is_some());
        assert_eq!(lock.system["g815"].image, "ghcr.io/ublue-os/bazzite-nvidia@sha256:ee");
        assert_eq!(lock.zypper["e1504g"].snapshot, "20261020");
        let again: Lock = toml::from_str(&toml::to_string(&lock).unwrap()).unwrap();
        assert_eq!(again, lock);
    }

    #[test]
    fn one_name_from_two_sources_keeps_both_entries() {
        let entry = |source: &str, version: &str| LockedPkg {
            name: "herdr".into(),
            source: source.into(),
            version: Some(version.into()),
            rev: None,
            patches: None,
            parts: Default::default(),
            artifacts: BTreeMap::new(),
        };
        let mut lock = Lock::default();
        lock.upsert(entry("github:ogulcancelik/herdr", "0.4.0"));
        lock.upsert(entry("brew:herdr", "0.4.1"));
        lock.upsert(entry("brew:herdr", "0.4.2"));
        assert_eq!(lock.pkg.len(), 2);
        assert_eq!(lock.pkg("herdr", "brew:herdr").unwrap().version.as_deref(), Some("0.4.2"));
        assert_eq!(lock.pkg("herdr", "github:ogulcancelik/herdr").unwrap().version.as_deref(), Some("0.4.0"));
    }

    #[test]
    fn two_updates_of_different_packages_both_land() {
        let entry = |name: &str, version: &str| LockedPkg {
            name: name.into(),
            source: format!("brew:{name}"),
            version: Some(version.into()),
            rev: None,
            patches: None,
            parts: Default::default(),
            artifacts: BTreeMap::new(),
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fspec.lock");
        let mut base = Lock::default();
        base.upsert(entry("a", "1"));
        base.upsert(entry("b", "1"));
        base.upsert(entry("gone", "1"));
        base.save(&path).unwrap();

        let mut first = base.clone();
        first.upsert(entry("a", "2"));
        first.pkg.retain(|p| p.name != "gone");
        let mut second = base.clone();
        second.upsert(entry("b", "2"));
        second.zypper.insert("e1504g".into(), LockedZypper { snapshot: "20261028".into() });
        first.save_over(&base, &path).unwrap();
        second.save_over(&base, &path).unwrap();

        let disk = Lock::load(&path).unwrap();
        let versions: Vec<_> =
            disk.pkg.iter().map(|p| format!("{}={}", p.name, p.version.as_deref().unwrap())).collect();
        assert_eq!(versions, ["a=2", "b=2"]);
        assert_eq!(disk.zypper["e1504g"].snapshot, "20261028");
    }
}
