use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::facts::Init;

/// One evaluated host: everything `plan` and `apply` need, with modules already flattened.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Host {
    pub name: String,
    /// The ssh alias `fspec apply` reaches this host by, default the host name.
    #[serde(default)]
    pub ssh: Option<String>,
    /// `<arch>-<os>`, e.g. `x86_64-linux`.
    pub platform: String,
    #[serde(default)]
    pub system: Option<System>,
    #[serde(default)]
    pub users: Vec<User>,
    #[serde(default)]
    pub resources: Vec<Resource>,
    /// Paths FormalSpec refuses to act under, e.g. `/home`.
    #[serde(default)]
    pub protect: Vec<String>,
    /// Paths kept when `fspec switch --reset-var` resets `/etc` and `/var`.
    #[serde(default)]
    pub persist: Vec<String>,
    /// Packages resolve to the newest release at least this old (`3d`, `12h`), unless bleeding.
    #[serde(default)]
    pub release_age: Option<String>,
    /// Machines that build this host's git packages when CI has no artifact for them, tried in
    /// order before building on the host itself.
    #[serde(default)]
    pub builders: Vec<Builder>,
    /// App data this host syncs with the other hosts that list an item of the same name.
    #[serde(default)]
    pub sync: Vec<SyncItem>,
    /// `LANG` for the whole system, e.g. `en_US.UTF-8`, generated where the distro needs it.
    #[serde(default)]
    pub locale: Option<String>,
    /// The keyboard layout, e.g. `us` or `de`, for the console and the desktop.
    #[serde(default)]
    pub keymap: Option<String>,
    /// A zoneinfo name such as `Europe/Madrid`.
    #[serde(default)]
    pub timezone: Option<String>,
    /// This host is a VM another machine runs, booted with `fspec vm start`.
    #[serde(default)]
    pub vm: Option<Vm>,
}

/// `sync(...)`: app data that moves between the user's hosts with `fspec sync`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SyncItem {
    pub name: String,
    /// The hosts that hold the item, this one included.
    pub peers: Vec<String>,
    /// Files or directories (ending in `/`) under `~/`.
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub keys: Option<JsonKeys>,
    #[serde(default)]
    pub keyring: Vec<KeyringItem>,
    #[serde(default)]
    pub mode: SyncMode,
    #[serde(default)]
    pub conflict: Conflict,
    /// Items that sync first; this one waits while one of them did not.
    #[serde(default)]
    pub after: Vec<String>,
    /// Run on a host after the item changed there; `{key}` is a `keys` value, shell-quoted.
    #[serde(default)]
    pub on_receive: Option<String>,
    /// Process names that hold the item open; it is skipped while one runs on either host.
    #[serde(default)]
    pub unless_running: Vec<String>,
}

/// `json_keys(path, [keys])`: only these top-level keys of a JSON file sync.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct JsonKeys {
    pub path: String,
    pub keys: Vec<String>,
}

/// Secret Service items: those with `schema` and every one of `attrs` (a value ending in `*`
/// matches a prefix), and `label` when given.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct KeyringItem {
    #[serde(default)]
    pub schema: Option<String>,
    #[serde(default)]
    pub attrs: BTreeMap<String, String>,
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SyncMode {
    /// Every host writes; each change goes to the peers.
    #[default]
    Live,
    /// Copied whole to a host that has none of it, then each host keeps its own.
    Seed,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Conflict {
    /// The later modification wins, and a change beats a deletion.
    #[default]
    Newest,
    /// Both copies stay and the sync reports it.
    Manual,
}

impl SyncItem {
    pub fn check(&self) -> anyhow::Result<()> {
        let what = format!("sync({})", self.name);
        let word = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !word(&self.name) {
            anyhow::bail!("{what}: the name takes letters, digits, `-` and `_`");
        }
        let json = self.keys.iter().map(|k| &k.path);
        for p in self.paths.iter().chain(json) {
            if !p.starts_with("~/") || p.len() < 3 || p.split('/').any(|c| c == ".." || c == ".") {
                anyhow::bail!("{what}: {p} must be a path under ~/");
            }
        }
        if self.paths.is_empty() && self.keyring.is_empty() && self.keys.is_none() {
            anyhow::bail!("{what}: nothing to sync; give paths, keys or keyring");
        }
        if self.peers.len() < 2 {
            anyhow::bail!("{what}: peers names every host that holds it, at least two");
        }
        for k in &self.keyring {
            if k.schema.is_none() && k.attrs.is_empty() {
                anyhow::bail!(
                    "{what}: a keyring item needs a schema or attrs, since the Secret Service searches by them"
                );
            }
        }
        if let Some(cmd) = &self.on_receive {
            for placeholder in cmd.split('{').skip(1).filter_map(|p| p.split_once('}')).map(|(p, _)| p) {
                if !self.keys.as_ref().is_some_and(|k| k.keys.iter().any(|k| k == placeholder)) {
                    anyhow::bail!("{what}: on_receive names {{{placeholder}}}, which is not one of its keys");
                }
            }
        }
        Ok(())
    }
}

/// A machine reached over ssh that builds packages for a host.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Builder {
    pub name: String,
    /// ssh destinations for the same machine, in order of preference (the Tailscale name, then
    /// the LAN address). Default: the name.
    #[serde(default)]
    pub routes: Vec<String>,
}

impl Builder {
    pub fn routes(&self) -> Vec<&str> {
        if self.routes.is_empty() { vec![self.name.as_str()] } else { self.routes.iter().map(String::as_str).collect() }
    }
}

/// `vm(...)`: where and how `fspec vm` runs a host that is a virtual machine. The guest starts
/// from the distro's cloud image and takes the rest from `fspec apply` like any other host.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Vm {
    /// The machine that runs it, as ssh reaches it (or this machine's host name).
    pub on: String,
    /// The port on `on`'s loopback that forwards to the guest's ssh.
    pub port: u16,
    #[serde(default = "vm_cpus")]
    pub cpus: u32,
    #[serde(default = "vm_memory")]
    pub memory_gib: u32,
    /// The size the disk grows to on first boot. It is sparse, so it takes what the guest writes.
    #[serde(default = "vm_disk")]
    pub disk_gib: u32,
    /// A qcow2 cloud image with cloud-init, default the distro's own.
    #[serde(default)]
    pub image: Option<String>,
    /// Keys the first user accepts over ssh from the first boot on.
    #[serde(default)]
    pub authorized_keys: Vec<String>,
}

fn vm_cpus() -> u32 {
    4
}

fn vm_memory() -> u32 {
    8
}

fn vm_disk() -> u32 {
    40
}

/// A resource as `plan` and the state file see it: flat, with the package that owns it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Desired {
    pub resource: Resource,
    /// The `pkg` this was declared under (`pkg(files = [...])`); removed together with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
}

impl Host {
    /// Every resource the host declares, flattened: the system layer, users, and each package's
    /// files, services and timers as resources of their own.
    pub fn desired(&self) -> Vec<Desired> {
        let mut all = Vec::new();
        let mut push = |resource, owner: Option<&str>| all.push(Desired { resource, owner: owner.map(Into::into) });
        match &self.system {
            Some(System::Image(i)) => push(Resource::Image(i.clone()), None),
            Some(System::Distro(d)) => push(Resource::Distro(d.clone()), None),
            None => {}
        }
        if self.locale.is_some() || self.keymap.is_some() || self.timezone.is_some() {
            let l =
                Locale { locale: self.locale.clone(), keymap: self.keymap.clone(), timezone: self.timezone.clone() };
            push(Resource::Locale(l), None);
        }
        for u in &self.users {
            push(Resource::User(u.clone()), None);
        }
        for r in &self.resources {
            let Resource::Pkg(p) = r else {
                push(r.clone(), None);
                continue;
            };
            let (files, services, timers) = (p.files.clone(), p.services.clone(), p.timers.clone());
            let bare = Pkg { files: vec![], services: vec![], timers: vec![], ..p.clone() };
            push(Resource::Pkg(bare), None);
            let owner = Some(p.name.as_str());
            files.into_iter().for_each(|f| push(Resource::File(f), owner));
            services.into_iter().for_each(|s| push(Resource::Service(s), owner));
            timers.into_iter().for_each(|t| push(Resource::Timer(t), owner));
        }
        all
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum System {
    Image(Image),
    Distro(Distro),
}

/// A classic distro whose package manager FormalSpec drives directly: `fedora(...)`, `arch(...)`, ….
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Distro {
    pub distro: String,
    pub init: Init,
    /// Base packages, named the way this distro names them.
    #[serde(default)]
    pub packages: Vec<String>,
    /// Kernel options added to `/etc/kernel/cmdline`, which the boot entries of a root that
    /// `fspec switch --beside` installed are written from (Tumbleweed only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kernel_args: Vec<String>,
}

/// The host's `locale`, `keymap` and `timezone`, applied as one resource.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Locale {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keymap: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
}

/// A third-party package repository on a zypper distro, e.g. 1Password's or Packman.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Repo {
    /// The repo's zypper alias.
    pub name: String,
    pub url: String,
    /// URL of the signing key, imported into rpm's keyring only when its fingerprint matches.
    #[serde(default)]
    pub key: Option<String>,
    /// Full fingerprint of `key`, hex as `gpg --show-keys` prints it.
    #[serde(default)]
    pub fingerprint: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
// A host holds a few hundred resources at most; boxing Pkg would buy nothing measurable.
#[allow(clippy::large_enum_variant)]
pub enum Resource {
    Pkg(Pkg),
    File(File),
    Service(Service),
    Timer(Timer),
    User(User),
    Secret(Secret),
    Defaults(Defaults),
    Setting(Setting),
    Image(Image),
    Distro(Distro),
    Repo(Repo),
    Locale(Locale),
}

/// Where a resource's record lives: the system state file or the user one.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    System,
    User,
}

impl Resource {
    pub fn kind(&self) -> &'static str {
        match self {
            Resource::Pkg(_) => "pkg",
            Resource::File(_) => "file",
            Resource::Service(_) => "service",
            Resource::Timer(_) => "timer",
            Resource::User(_) => "user",
            Resource::Secret(_) => "secret",
            Resource::Defaults(_) => "defaults",
            Resource::Setting(_) => "setting",
            Resource::Image(_) => "image",
            Resource::Distro(_) => "distro",
            Resource::Repo(_) => "repo",
            Resource::Locale(_) => "locale",
        }
    }

    /// Identity within a host. Two resources with the same key are the same thing on disk.
    pub fn key(&self) -> Key {
        let id = match self {
            Resource::Pkg(p) => p.name.clone(),
            Resource::File(f) => f.path.clone(),
            Resource::Service(s) => s.name.clone(),
            Resource::Timer(t) => t.name.clone(),
            Resource::User(u) => u.name.clone(),
            Resource::Secret(s) => s.path.clone(),
            Resource::Defaults(d) => format!("{} {}", d.target(), d.key),
            Resource::Setting(s) => format!("{} {}", s.tool.name(), s.key),
            Resource::Repo(r) => r.name.clone(),
            Resource::Image(_) | Resource::Distro(_) | Resource::Locale(_) => "system".into(),
        };
        Key { kind: self.kind().into(), id }
    }

    pub fn scope(&self) -> Scope {
        let home = |p: &str| if p.starts_with('~') { Scope::User } else { Scope::System };
        match self {
            Resource::File(f) => home(&f.path),
            Resource::Secret(s) => home(&s.path),
            Resource::Service(s) if s.user => Scope::User,
            Resource::Timer(t) if t.user => Scope::User,
            Resource::Defaults(d) => {
                if d.system {
                    Scope::System
                } else {
                    Scope::User
                }
            }
            // Homebrew refuses to run as root.
            Resource::Pkg(Pkg { source: Source::Brew { .. }, .. }) => Scope::User,
            _ => Scope::System,
        }
    }
}

/// `kind:id`, the key used in state files and plan output.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(into = "String", try_from = "String")]
pub struct Key {
    pub kind: String,
    pub id: String,
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}:{}", self.kind, self.id)
    }
}

impl From<Key> for String {
    fn from(k: Key) -> String {
        k.to_string()
    }
}

impl TryFrom<String> for Key {
    type Error = String;
    fn try_from(s: String) -> Result<Key, String> {
        let (kind, id) = s.split_once(':').ok_or_else(|| format!("state key `{s}` has no kind"))?;
        Ok(Key { kind: kind.into(), id: id.into() })
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Pkg {
    pub name: String,
    pub source: Source,
    /// An exact upstream version for `fspec update` to lock instead of the newest release.
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub mode: PkgMode,
    #[serde(default)]
    pub channel: Channel,
    #[serde(default)]
    pub patches: Vec<String>,
    #[serde(default)]
    pub build_flags: Vec<String>,
    /// Replaces recipe detection for a `git(...)` package: a command that installs into
    /// `$FSPEC_OUT`, run with `$FSPEC_SRC` set to the patched checkout.
    #[serde(default)]
    pub build: Vec<String>,
    /// The commands to link, by file name: a source build packs only these, and any store
    /// package links only these of the executables it found. Empty: all of them.
    #[serde(default)]
    pub bins: Vec<String>,
    /// More command names for one of the package's executables, for tools that act on the name
    /// they run as: `{"cargo": "rustup"}` links `cargo` to rustup's proxy.
    #[serde(default)]
    pub links: BTreeMap<String, String>,
    /// Data trees linked where the system looks for them: a path under `/usr/local/share`
    /// (`icons/Bibata-Modern-Classic`) to a path inside the package (`.` for its root). A package
    /// with `share` may have no executables: fonts, icon, cursor and GTK themes.
    #[serde(default)]
    pub share: BTreeMap<String, String>,
    /// Config linked into `/etc`: a path under it (`dbus-1/system.d/asusd.conf`) to a path inside
    /// the package. Linux only.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub etc: BTreeMap<String, String>,
    /// More `url(...)` archives unpacked into the package: a path inside it to the source. Paths
    /// are relative to the package root, the archive's single top-level directory when it has one.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub parts: BTreeMap<String, Source>,
    /// macOS app bundles inside the package (`Kopuz.app`, `bin/kopuz.app`), cloned into
    /// /Applications under their own name. A package with `apps` may have no executables.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub apps: Vec<String>,
    /// App data paths removed with the package.
    #[serde(default)]
    pub data: Vec<String>,
    #[serde(default)]
    pub keep_data: bool,
    /// Files, services and timers that belong to this package and go when it goes.
    #[serde(default)]
    pub files: Vec<File>,
    #[serde(default)]
    pub services: Vec<Service>,
    #[serde(default)]
    pub timers: Vec<Timer>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PkgMode {
    #[default]
    Pinned,
    SelfManaged,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    #[default]
    Stable,
    Bleeding,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Source {
    Aqua {
        name: String,
    },
    Github {
        repo: String,
        /// Glob over release asset names when the per-platform match is ambiguous.
        #[serde(default)]
        asset: Option<String>,
    },
    Npm {
        name: String,
    },
    Cargo {
        name: String,
    },
    Flatpak {
        #[serde(default)]
        id: Option<String>,
        #[serde(default = "flathub")]
        remote: String,
    },
    Brew {
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        cask: bool,
        /// A third-party tap (`felixkratz/formulae`), tapped before the install. The item is
        /// installed by its full name, which is how Homebrew trusts one item of a tap.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tap: Option<String>,
    },
    Dnf(DistroPkgs),
    Pacman(DistroPkgs),
    Apt(DistroPkgs),
    Xbps(DistroPkgs),
    Moss(DistroPkgs),
    Zypper(DistroPkgs),
    Git {
        url: String,
        #[serde(default = "main")]
        rev: String,
        /// Packages the build needs, installed with the image's package manager before the
        /// recipe runs in it.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        deps: Vec<String>,
        /// The OCI image the build runs in. Unset: on the build machine itself.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        image: Option<String>,
        /// Files or directories of the repository that move the lock: a branch `rev` locks its
        /// newest commit touching one of them, not its tip. Empty: the tip.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        paths: Vec<String>,
    },
    Nix {
        attr: String,
    },
    /// One file or archive at a fixed URL, pinned by its hash.
    Url {
        url: String,
    },
    /// CPython from python-build-standalone, at the build uv's download metadata names for the
    /// version.
    Python,
    /// A PyPI distribution and its dependencies, installed into a virtualenv over `python`.
    Pypi {
        name: String,
    },
}

fn opt(name: &Option<String>) -> String {
    name.as_ref().map(|n| format!(":{n}")).unwrap_or_default()
}

fn flathub() -> String {
    "flathub".into()
}

fn main() -> String {
    "main".into()
}

impl Source {
    pub fn kind(&self) -> &'static str {
        match self {
            Source::Aqua { .. } => "aqua",
            Source::Github { .. } => "github",
            Source::Npm { .. } => "npm",
            Source::Cargo { .. } => "cargo",
            Source::Flatpak { .. } => "flatpak",
            Source::Brew { .. } => "brew",
            Source::Dnf(_) => "dnf",
            Source::Pacman(_) => "pacman",
            Source::Apt(_) => "apt",
            Source::Xbps(_) => "xbps",
            Source::Moss(_) => "moss",
            Source::Zypper(_) => "zypper",
            Source::Git { .. } => "git",
            Source::Nix { .. } => "nix",
            Source::Url { .. } => "url",
            Source::Python => "python",
            Source::Pypi { .. } => "pypi",
        }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let kind = self.kind();
        match self {
            Source::Aqua { name } | Source::Npm { name } | Source::Cargo { name } | Source::Pypi { name } => {
                write!(f, "{kind}:{name}")
            }
            Source::Url { url } => write!(f, "{kind}:{url}"),
            Source::Python => write!(f, "{kind}"),
            Source::Github { repo, .. } => write!(f, "{kind}:{repo}"),
            Source::Flatpak { id, .. } => write!(f, "{kind}{}", opt(id)),
            Source::Brew { name, cask, tap } => {
                let tap = tap.as_ref().map(|t| format!(":{t}")).unwrap_or_default();
                write!(f, "{kind}{}{tap}{}", if *cask { ":cask" } else { "" }, opt(name))
            }
            Source::Dnf(d)
            | Source::Pacman(d)
            | Source::Apt(d)
            | Source::Xbps(d)
            | Source::Moss(d)
            | Source::Zypper(d) => {
                write!(f, "{kind}:{}", d.names.join(","))
            }
            Source::Git { url, rev, .. } => write!(f, "{kind}:{url}@{rev}"),
            Source::Nix { attr } => write!(f, "{kind}:{attr}"),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DistroPkgs {
    /// Empty means "the package's own name" when used as a `pkg` source.
    #[serde(default, deserialize_with = "one_or_many")]
    pub names: Vec<String>,
    /// The `repo(...)` the packages come from (zypper only). While that repo is deferred, so
    /// are they, instead of failing the apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
}

fn one_or_many<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match Option::<OneOrMany>::deserialize(d)? {
        None => Vec::new(),
        Some(OneOrMany::One(s)) => vec![s],
        Some(OneOrMany::Many(v)) => v,
    })
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct File {
    /// Absolute, or starting with `~/` for a path in the user's home.
    pub path: String,
    /// Repo-relative source file, or directory for a link.
    #[serde(default)]
    pub src: Option<String>,
    /// `link` mode only: what the link points at outside the repo, absolute or `~/…`. It may be
    /// a directory, and it may not exist (a drive that is not mounted yet).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// A string is written as-is; any other value is serialized to `format`.
    #[serde(default)]
    pub content: Option<Value>,
    /// Inferred from the path's extension when unset.
    #[serde(default)]
    pub format: Option<Format>,
    #[serde(default)]
    pub mode: FileMode,
    /// `patch` mode: dotted key path to desired value or list strategy.
    #[serde(default)]
    pub owns: BTreeMap<String, Value>,
    #[serde(default)]
    pub on_conflict: OnConflict,
    #[serde(default)]
    pub on_drift: OnDrift,
    /// Octal permission bits, e.g. `"0644"`.
    #[serde(default)]
    pub perms: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FileMode {
    #[default]
    Managed,
    Link,
    Seed,
    Patch,
    /// The path must not exist: a file or link there is removed, and nothing is written.
    Absent,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    Text,
    Json,
    Toml,
    Yaml,
    Ini,
    Plist,
    Lua,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OnConflict {
    #[default]
    Ours,
    Theirs,
    Fail,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OnDrift {
    #[default]
    Overwrite,
    Fail,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Service {
    pub name: String,
    /// The command to run. Without one the service is a unit the distro ships, which fspec only
    /// enables.
    #[serde(default)]
    pub exec: Option<String>,
    #[serde(default)]
    pub user: bool,
    #[serde(default)]
    pub restart: Restart,
    /// Abstract dependencies (`network`, another service's name), mapped per init.
    #[serde(default)]
    pub after: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub description: Option<String>,
    /// systemd-only directives. `plan` fails on any other init when this is set.
    #[serde(default)]
    pub systemd: BTreeMap<String, Value>,
    /// launchd-only plist keys, merged over the ones fspec writes (`RunAtLoad = False` wins).
    /// `plan` fails on any other init when this is set.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub launchd: BTreeMap<String, Value>,
    /// A distro unit that is the login screen: on systemd the host boots to graphical.target
    /// while it is declared.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub display_manager: bool,
}

impl Service {
    /// The declared command; empty for a distro unit, which never gets rendered.
    pub fn exec(&self) -> &str {
        self.exec.as_deref().unwrap_or_default()
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Restart {
    #[default]
    No,
    OnFailure,
    Always,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Timer {
    pub name: String,
    pub exec: String,
    /// Five-field cron expression, or `@hourly`, `@daily`, `@weekly`, `@monthly`.
    pub schedule: String,
    #[serde(default)]
    pub user: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct User {
    pub name: String,
    #[serde(default)]
    pub uid: Option<u32>,
    #[serde(default)]
    pub shell: Option<String>,
    #[serde(default)]
    pub groups: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Secret {
    pub path: String,
    /// Repo-relative `.age` file.
    pub src: String,
    #[serde(default = "secret_perms")]
    pub perms: String,
    /// `user` or `user:group`.
    #[serde(default)]
    pub owner: Option<String>,
    /// agenix's `identityPaths`: SSH private keys or age identity files tried in order. Empty
    /// means the host's SSH keys for a system secret, the user's for a user one, then
    /// `~/.config/fspec/age.key`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub identities: Vec<String>,
}

fn secret_perms() -> String {
    "0400".into()
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    pub domain: String,
    pub key: String,
    pub value: Value,
    /// The machine-wide domain in /Library/Preferences, written as root.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub system: bool,
}

impl Defaults {
    /// The domain as the `defaults` command takes it.
    pub fn target(&self) -> String {
        if self.system { format!("/Library/Preferences/{}", self.domain) } else { self.domain.clone() }
    }
}

/// A macOS system setting that has a command and no defaults domain: `pmset -a <key>`,
/// `systemsetup -set<key>`, or the application firewall's `socketfilterfw --set<key>`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Setting {
    pub tool: SettingTool,
    pub key: String,
    pub value: Value,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SettingTool {
    Pmset,
    Systemsetup,
    Firewall,
}

impl SettingTool {
    pub fn name(self) -> &'static str {
        match self {
            SettingTool::Pmset => "pmset",
            SettingTool::Systemsetup => "systemsetup",
            SettingTool::Firewall => "firewall",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct Image {
    pub base: String,
    /// COPR projects (`owner/project`) enabled before the packages install.
    #[serde(default)]
    pub copr: Vec<String>,
    /// URLs of `.repo` files added before the packages install.
    #[serde(default)]
    pub repos: Vec<String>,
    /// Distro packages layered into the image.
    #[serde(default)]
    pub packages: Option<Source>,
    /// Containerfile build stages placed before the image's own `FROM`, for what is built from
    /// source and copied in by `steps`.
    #[serde(default)]
    pub stages: Vec<String>,
    /// Containerfile instructions run after the packages install.
    #[serde(default)]
    pub steps: Vec<String>,
    #[serde(default)]
    pub kernel_args: Vec<String>,
    /// Derived from the base image's name when unset (`bazzite-nvidia` is `bazzite`).
    #[serde(default)]
    pub distro: Option<String>,
}

impl Image {
    pub fn distro(&self) -> String {
        if let Some(d) = &self.distro {
            return d.clone();
        }
        let repo = self.base.rsplit('/').next().unwrap_or(&self.base);
        let repo = repo.split(['@', ':']).next().unwrap_or(repo);
        repo.split('-').next().unwrap_or(repo).to_string()
    }
}

/// List strategies for `patch` mode, as written in `owns`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ListStrategy {
    ListUnion { items: Vec<Value> },
    ListReplace { items: Vec<Value> },
    ListKeyed { field: String, items: Vec<Value> },
}
