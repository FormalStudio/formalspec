//! `fspec app`: installs and uninstalls that land in the spec, for FormalShell's launcher
//! (docs/shell-install.md). An install is a commit to the spec's `apps.star`, applied for this
//! host and then pushed; nothing is installed behind the spec's back.

use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use fspec_core::activity::{self, Kind, Operation, Phase};
use fspec_core::facts::local_hostname;
use fspec_core::resource::{Pkg, Source};
use fspec_core::runner::{Cmd, LiveRunner, Runner};
use fspec_core::{Host, Lock, Paths, Resource, Scope, State};
use serde::{Deserialize, Serialize};

use crate::{Pinned, ScopeArg, apply_local, remote_alias, state_dir};

pub const APPS: &str = "apps.star";
pub const MODULE: &str = "modules/apps.star";
/// Where an install's commit waits for the push a network failure stopped.
fn pending_ref(host: &str) -> String {
    format!("refs/fspec/apps/{host}")
}

#[derive(Subcommand)]
pub enum AppCommand {
    /// Write apps.star and modules/apps.star next to the spec, for a host to load.
    Init,
    /// Add a package to this host's apps.star by its search id (`flatpak:org.mozilla.firefox`,
    /// `zypper:btop`), commit that, apply it for this host and push the commit.
    Install {
        id: String,
        /// The host whose list it goes in, default this machine; another host is applied over ssh.
        #[arg(long)]
        host: Option<String>,
        /// Start it in a process of its own, print `{"op":"<id>"}` once it publishes on the
        /// operation stream, and return.
        #[arg(long)]
        detach: bool,
        #[arg(long, hide = true)]
        op: Option<String>,
    },
    /// Take a package out of apps.star the same way. One the spec declares anywhere else is
    /// refused.
    #[command(alias = "remove")]
    Uninstall {
        id: String,
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        detach: bool,
        #[arg(long, hide = true)]
        op: Option<String>,
    },
    /// Make the inverse of the last `fspec app` change on the host, or the last one of ID: an
    /// install is uninstalled, an uninstall installed again.
    Undo {
        id: Option<String>,
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        detach: bool,
        #[arg(long, hide = true)]
        op: Option<String>,
    },
    /// Which package put an installed file there (a path, or a desktop file id such as
    /// `org.mozilla.firefox`), and whether `fspec app uninstall` can take it out.
    Owner {
        target: String,
        #[arg(long)]
        json: bool,
    },
    /// Record COMMIT as the last `fspec app` change of this machine; run over ssh by an install
    /// with `--host`.
    #[command(hide = true)]
    Mark { commit: String },
    /// This host's apps.star entries.
    List {
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

/// One app in `apps.star`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The `pkg(...)` name, unique per host.
    pub name: String,
    /// `flatpak`, `zypper`, `aqua`, ...: the source builtin the module calls.
    pub source: String,
    /// What that builtin takes: the Flathub id, the distro package, the npm name.
    #[serde(rename = "ref")]
    pub reference: String,
    /// The day it was installed, `YYYY-MM-DD`.
    pub added: String,
}

impl Entry {
    pub fn id(&self) -> String {
        format!("{}:{}", self.source, self.reference)
    }
}

pub type Apps = BTreeMap<String, Vec<Entry>>;

/// Sources the module writes, per platform family.
const LINUX: &[&str] = &["flatpak", "zypper", "pacman", "apt", "dnf", "aqua", "github", "npm", "cargo"];
const MACOS: &[&str] = &["brew", "cask", "aqua", "github", "npm", "cargo"];

/// Whether `fspec app install` can write `source` for a host of `platform` running `distro`.
pub fn installable(source: &str, platform: &str, distro: &str) -> bool {
    if platform.ends_with("-darwin") {
        return MACOS.contains(&source);
    }
    match distro_manager(distro) {
        Some(m) if source == m => true,
        _ => LINUX.contains(&source) && !DISTROS.contains(&source),
    }
}

const DISTROS: &[&str] = &["zypper", "pacman", "apt", "dnf"];

fn distro_manager(distro: &str) -> Option<&'static str> {
    if distro.contains("suse") {
        Some("zypper")
    } else if matches!(distro, "arch" | "archarm" | "cachyos" | "endeavouros" | "manjaro") {
        Some("pacman")
    } else if matches!(distro, "debian" | "ubuntu" | "linuxmint" | "pop" | "raspbian") {
        Some("apt")
    } else if matches!(distro, "fedora" | "rhel" | "centos" | "rocky" | "almalinux") {
        Some("dnf")
    } else {
        None
    }
}

/// The entry a search id makes, `added` today.
pub fn entry(id: &str, today: &str) -> Result<Entry> {
    let Some((source, reference)) = id.split_once(':') else {
        bail!("`{id}` is not a search id; take one from `fspec search --json`, such as flatpak:org.mozilla.firefox")
    };
    if reference.is_empty() || reference.chars().any(|c| c.is_whitespace() || c.is_control()) {
        bail!("`{id}` names no package");
    }
    if !LINUX.contains(&source) && !MACOS.contains(&source) {
        bail!("fspec app installs {} packages, not {source}", [LINUX, MACOS].concat().join(", "));
    }
    let last = reference.rsplit('/').next().unwrap_or(reference);
    let name = match source {
        "npm" | "github" | "aqua" => last,
        _ => reference,
    };
    if source == "flatpak" && !crate::run::flatpak::is_app_id(reference) {
        bail!("`{reference}` is not a Flathub app id like org.mozilla.firefox");
    }
    Ok(Entry { name: name.into(), source: source.into(), reference: reference.into(), added: today.into() })
}

/// `apps.star` as `fspec app` writes it: one assignment of a JSON-shaped literal, which is valid
/// Starlark and reads back without evaluating anything.
pub fn render(apps: &Apps) -> Result<String> {
    let mut buf = Vec::new();
    let mut ser =
        serde_json::Serializer::with_formatter(&mut buf, serde_json::ser::PrettyFormatter::with_indent(b"    "));
    apps.serialize(&mut ser)?;
    Ok(format!(
        "# Written by `fspec app install` and `uninstall`, whole, so edits here are not kept. \
         {MODULE} turns it into packages.\nAPPS = {}\n",
        String::from_utf8(buf)?
    ))
}

pub fn parse(text: &str) -> Result<Apps> {
    let body = text
        .find("APPS = ")
        .map(|i| &text[i + "APPS = ".len()..])
        .context("apps.star has no `APPS = ` line; it is written by `fspec app`, not by hand")?;
    serde_json::from_str(body).context("apps.star is not in the shape `fspec app` writes")
}

/// `modules/apps.star`, written once by `fspec app init` and the spec author's from then on.
pub const MODULE_TEXT: &str = r#"# The apps installed from the shell. `fspec app` keeps the list in //apps.star; a host gets
# its own entries by adding `apps` to its modules.
load("//apps.star", "APPS")

def _source(a):
    s, r = a["source"], a["ref"]
    if s == "flatpak":
        return flatpak(r)
    if s == "zypper":
        return zypper(r)
    if s == "pacman":
        return pacman(r)
    if s == "apt":
        return apt(r)
    if s == "dnf":
        return dnf(r)
    if s == "aqua":
        return aqua(r)
    if s == "github":
        return github(r)
    if s == "npm":
        return npm(r)
    if s == "cargo":
        return cargo(r)
    if s == "brew":
        return brew(r)
    if s == "cask":
        return brew(r, cask = True)
    fail("apps.star: no source " + s)

def apps(host):
    return [pkg(a["name"], source = _source(a)) for a in APPS.get(host.name, [])]
"#;

/// Whether `p` is the package `e` names, however the spec declared it.
pub fn same(p: &Pkg, source: &str, reference: &str) -> bool {
    let own =
        |names: &[String]| if names.is_empty() { p.name == reference } else { names.iter().any(|n| n == reference) };
    match &p.source {
        Source::Flatpak { id, .. } => source == "flatpak" && id.as_deref().unwrap_or(&p.name) == reference,
        Source::Zypper(d) => source == "zypper" && own(&d.names),
        Source::Pacman(d) => source == "pacman" && own(&d.names),
        Source::Apt(d) => source == "apt" && own(&d.names),
        Source::Dnf(d) => source == "dnf" && own(&d.names),
        Source::Aqua { name } => source == "aqua" && (name == reference || p.name == reference),
        Source::Github { repo, .. } => source == "github" && repo == reference,
        Source::Npm { name } => source == "npm" && name == reference,
        Source::Cargo { name } => source == "cargo" && name == reference,
        Source::Brew { name, cask, .. } => {
            (source == "cask") == *cask
                && matches!(source, "brew" | "cask")
                && name.as_deref().unwrap_or(&p.name) == reference
        }
        _ => false,
    }
}

fn pkgs(host: &Host) -> impl Iterator<Item = &Pkg> {
    host.resources.iter().filter_map(|r| match r {
        Resource::Pkg(p) => Some(p),
        _ => None,
    })
}

/// The day `secs` after the epoch falls on, `YYYY-MM-DD` in UTC.
pub fn day(secs: u64) -> String {
    // Howard Hinnant's civil_from_days.
    let z = (secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

fn today() -> String {
    day(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default())
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Edit {
    Install,
    Uninstall,
}

impl Edit {
    fn verb(self) -> &'static str {
        match self {
            Edit::Install => "install",
            Edit::Uninstall => "uninstall",
        }
    }
}

/// The new list for `host`, or why the edit is refused. `declared` is every package the host's
/// spec declares at the base commit, `apps.star`'s included.
pub fn edited(apps: &Apps, host: &str, e: &Entry, edit: Edit, declared: &[&Pkg]) -> Result<Apps> {
    let mut apps = apps.clone();
    let list = apps.entry(host.to_string()).or_default();
    let listed = list.iter().position(|a| a.source == e.source && a.reference == e.reference);
    match (edit, listed) {
        (Edit::Install, Some(_)) => bail!("{} is already in {host}'s apps.star", e.id()),
        (Edit::Install, None) => {
            if let Some(p) = declared.iter().find(|p| p.name == e.name || same(p, &e.source, &e.reference)) {
                bail!("host {host} already declares {} ({}); it is installed by the spec", p.name, p.source);
            }
            list.push(e.clone());
            list.sort_by(|a, b| a.name.cmp(&b.name));
        }
        (Edit::Uninstall, Some(i)) => {
            list.remove(i);
        }
        (Edit::Uninstall, None) => match declared.iter().find(|p| same(p, &e.source, &e.reference)) {
            Some(p) => bail!(
                "{} ({}) is declared in host {host}'s spec outside apps.star; take it out there",
                p.name,
                p.source
            ),
            None => bail!("{} is not in host {host}'s apps.star", e.id()),
        },
    }
    if apps.get(host).is_some_and(Vec::is_empty) {
        apps.remove(host);
    }
    Ok(apps)
}

/// The spec's repository and the branch installs are pushed to.
struct Repo {
    top: PathBuf,
    /// The spec directory inside it, `""` at the top.
    rel: String,
    remote: String,
    branch: String,
}

fn git(top: &Path) -> Cmd {
    Cmd::new("git").arg("-C").arg(top.display().to_string())
}

impl Repo {
    fn find(spec_dir: &Path) -> Result<Repo> {
        let r = &LiveRunner;
        let top = r
            .check(&git(spec_dir).args(["rev-parse", "--show-toplevel"]))
            .with_context(|| format!("{} is not in a git repository, which installs commit to", spec_dir.display()))?;
        let top = PathBuf::from(top.stdout.trim());
        let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let rel =
            canon(spec_dir).strip_prefix(canon(&top)).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        let up = r.run(&git(&top).args(["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{upstream}"]))?;
        let Some((remote, branch)) = up.stdout.trim().split_once('/').filter(|_| up.status == 0) else {
            bail!("the spec checkout's branch has no upstream to push installs to (`git branch -u origin/main`)")
        };
        Ok(Repo { top, rel, remote: remote.into(), branch: branch.into() })
    }

    fn path(&self, file: &str) -> String {
        match self.rel.as_str() {
            "" => file.to_string(),
            rel => format!("{rel}/{file}"),
        }
    }

    fn rev(&self, rev: &str) -> Option<String> {
        let out = LiveRunner
            .run(&git(&self.top).args(["rev-parse", "--verify", "--quiet"]).arg(format!("{rev}^{{commit}}")))
            .ok()?;
        (out.status == 0).then(|| out.stdout.trim().to_string())
    }

    fn contains(&self, ancestor: &str, of: &str) -> bool {
        LiveRunner.run(&git(&self.top).args(["merge-base", "--is-ancestor", ancestor, of])).is_ok_and(|o| o.status == 0)
    }

    /// The upstream tip after a fetch, or the last fetched one when the fetch fails, then a commit
    /// of this host's a push has not taken yet when it is built on that.
    fn base(&self, host: &str) -> Result<String> {
        let fetched = LiveRunner.run(&git(&self.top).args(["fetch", "--quiet", &self.remote, &self.branch]))?;
        if fetched.status != 0 {
            eprintln!(
                "fspec: fetching {} failed, building on the last fetched tip: {}",
                self.remote,
                fetched.stderr.trim()
            );
        }
        let tip = self
            .rev(&format!("refs/remotes/{}/{}", self.remote, self.branch))
            .with_context(|| format!("{}/{} has never been fetched", self.remote, self.branch))?;
        Ok(match self.rev(&pending_ref(host)) {
            Some(p) if self.contains(&tip, &p) => p,
            Some(p) if !self.contains(&p, &tip) => bail!(
                "{pending} holds an install that was never pushed, and {r}/{b} has moved on without it; put it \
                 on the tip, merged so this host's record of it still holds, and push it (`git switch -c apps-{host} {pending} && git merge {r}/{b} && git push \
                 {r} HEAD:{b} && git update-ref -d {pending}`), or drop it with `git update-ref -d {pending}`",
                pending = pending_ref(host),
                r = self.remote,
                b = self.branch,
            ),
            _ => tip,
        })
    }

    fn show(&self, rev: &str, file: &str) -> Result<Option<String>> {
        let out = LiveRunner.run(&git(&self.top).arg("show").arg(format!("{rev}:{}", self.path(file))))?;
        Ok((out.status == 0).then_some(out.stdout))
    }

    /// `base`'s tree unpacked into `dir`.
    fn unpack(&self, base: &str, dir: &Path) -> Result<()> {
        let tar = dir.join("spec.tar");
        let tree = dir.join("tree");
        std::fs::create_dir_all(&tree)?;
        LiveRunner
            .check(&git(&self.top).args(["archive", "--format=tar", "-o"]).arg(tar.display().to_string()).arg(base))?;
        LiveRunner.check(
            &Cmd::new("tar").args(["-xf"]).arg(tar.display().to_string()).arg("-C").arg(tree.display().to_string()),
        )?;
        Ok(())
    }

    /// A commit on `base` with `files` (spec-relative name, new content) replaced, written through a
    /// throwaway index so no ref moves and the checkout's own index and edits are never touched.
    fn commit(&self, base: &str, files: &[(&str, &Path)], message: &str, scratch: &Path) -> Result<String> {
        let index = scratch.join("index").display().to_string();
        let ix = |c: Cmd| c.env("GIT_INDEX_FILE", index.clone());
        LiveRunner.check(&ix(git(&self.top).args(["read-tree", base])))?;
        for (file, content) in files {
            let path = self.path(file);
            let sha = LiveRunner
                .check(&git(&self.top).args(["hash-object", "-w", "--path", &path]).arg(content.display().to_string()))?
                .stdout
                .trim()
                .to_string();
            LiveRunner.check(&ix(git(&self.top)
                .args(["update-index", "--add", "--cacheinfo"])
                .arg(format!("100644,{sha},{path}"))))?;
        }
        let tree = LiveRunner.check(&ix(git(&self.top).arg("write-tree")))?.stdout.trim().to_string();
        let out = LiveRunner.check(&git(&self.top).args(["commit-tree", &tree, "-p", base, "-m", message]))?;
        Ok(out.stdout.trim().to_string())
    }
}

enum Pushed {
    Done,
    /// The branch moved on: build the edit again on its new tip.
    Rejected,
    Offline(String),
}

impl Repo {
    fn push(&self, commit: &str) -> Result<Pushed> {
        let out = LiveRunner.run(
            &git(&self.top).args(["push", "--quiet", &self.remote]).arg(format!("{commit}:refs/heads/{}", self.branch)),
        )?;
        let err = out.stderr.trim().to_string();
        Ok(match out.status {
            0 => Pushed::Done,
            _ if ["rejected", "non-fast-forward", "fetch first"].iter().any(|w| err.contains(w)) => Pushed::Rejected,
            _ => Pushed::Offline(err),
        })
    }

    /// Before a change, bring the checkout to `base` (the upstream tip, or this host's unpushed
    /// commit on it), so the machine's own copy of the spec is never older than what it applies.
    /// A checkout with commits of its own, or an edit in a file the fast-forward changes, is
    /// refused rather than merged.
    fn sync(&self, base: &str) -> Result<()> {
        let head = LiveRunner.run(&git(&self.top).args(["symbolic-ref", "--short", "-q", "HEAD"]))?;
        if head.status != 0 || head.stdout.trim() != self.branch {
            return Ok(());
        }
        if !self.contains("HEAD", base) {
            bail!(
                "{} in {} has commits {}/{} does not; push them (or take them off the branch) before `fspec app`",
                self.branch,
                self.top.display(),
                self.remote,
                self.branch
            );
        }
        let out = LiveRunner.run(&git(&self.top).args(["merge", "--ff-only", "--quiet", base]))?;
        if out.status != 0 {
            bail!("fast-forwarding {} in {} failed: {}", self.branch, self.top.display(), out.stderr.trim());
        }
        Ok(())
    }

    /// Fast-forward the checked-out branch onto `commit` when it tracks the pushed branch and has
    /// nothing of its own; git refuses where an uncommitted edit would be overwritten, and that
    /// edit is left as it is.
    fn catch_up(&self, commit: &str) {
        let head = LiveRunner.run(&git(&self.top).args(["symbolic-ref", "--short", "-q", "HEAD"]));
        if !head.is_ok_and(|h| h.status == 0 && h.stdout.trim() == self.branch) || !self.contains("HEAD", commit) {
            return;
        }
        match LiveRunner.run(&git(&self.top).args(["merge", "--ff-only", "--quiet", commit])) {
            Ok(o) if o.status == 0 => {}
            Ok(o) => eprintln!("fspec: left {} as it is: {}", self.branch, o.stderr.trim()),
            Err(e) => eprintln!("fspec: left {} as it is: {e:#}", self.branch),
        }
    }
}

/// What `fspec app` last pushed from this machine, read by `fspec facts` so an apply from another
/// checkout can refuse a spec that does not have it.
pub fn record(paths: &Paths, commit: &str) -> Result<()> {
    let path = paths.user_state_dir().join("apps.json");
    std::fs::create_dir_all(paths.user_state_dir())?;
    fspec_core::flock::write_atomic(&path, &serde_json::to_vec(&serde_json::json!({ "commit": commit }))?)
        .with_context(|| format!("writing {}", path.display()))
}

pub fn run(paths: &Paths, spec_path: &Path, cmd: &AppCommand) -> Result<()> {
    let spec_dir = spec_path.parent().context("spec has no parent directory")?;
    match cmd {
        AppCommand::Init => init(spec_dir),
        AppCommand::Install { id, host, detach, op } => {
            change(paths, spec_path, id, host.as_deref(), Edit::Install, *detach, op.clone())
        }
        AppCommand::Uninstall { id, host, detach, op } => {
            change(paths, spec_path, id, host.as_deref(), Edit::Uninstall, *detach, op.clone())
        }
        AppCommand::Undo { id, host, detach, op } => {
            let host = host.clone().unwrap_or_else(local_hostname);
            // Detached before the inverse is looked up, since that fetches.
            if *detach {
                return self::detach(spec_path, &["undo"], id.as_deref(), &host);
            }
            // Published before the inverse is looked up, since that fetches; the change runs
            // inside this operation.
            let mut undo = Operation::new(
                Kind::Undo,
                &format!("fspec:undo:{host}"),
                format!("Undoing the last change on {host}"),
                Some(&host),
            );
            if let Some(given) = op {
                undo.id = given.clone();
            }
            let op_id = undo.id.clone();
            activity::run(undo, || {
                activity::phase(Phase::Resolving, None);
                let (edit, id) = last_change(spec_dir, &host, id.as_deref())?;
                eprintln!("fspec: undoing with `fspec app {} {id}`", edit.verb());
                activity::retitle(title(edit, &entry(&id, &today())?, paths));
                change(paths, spec_path, &id, Some(&host), edit, false, Some(op_id))
            })
        }
        AppCommand::Owner { target, json } => {
            let o = owner(paths, spec_path, target)?;
            if *json {
                println!("{}", serde_json::to_string(&o)?);
            } else {
                match (&o.id, o.owner) {
                    (Some(id), Some(by)) => println!(
                        "{}: {id}, installed by {by}{}",
                        o.path,
                        if o.removable { ", `fspec app uninstall` removes it" } else { "" }
                    ),
                    _ => println!("{}: no package owns it", o.path),
                }
            }
            Ok(())
        }
        AppCommand::Mark { commit } => record(paths, commit),
        AppCommand::List { host, json } => {
            let host = host.clone().unwrap_or_else(local_hostname);
            let text = std::fs::read_to_string(spec_dir.join(APPS))
                .with_context(|| format!("no {APPS} beside {}; run `fspec app init`", spec_path.display()))?;
            let apps = parse(&text)?;
            for e in apps.get(&host).into_iter().flatten() {
                match json {
                    true => println!("{}", serde_json::json!({"id": e.id(), "name": e.name, "added": e.added})),
                    false => println!("{}  {}  added {}", e.id(), e.name, e.added),
                }
            }
            Ok(())
        }
    }
}

fn init(spec_dir: &Path) -> Result<()> {
    let apps = spec_dir.join(APPS);
    if !apps.exists() {
        std::fs::write(&apps, render(&Apps::new())?)?;
        println!("wrote {}", apps.display());
    }
    let module = spec_dir.join(MODULE);
    if !module.exists() {
        std::fs::create_dir_all(module.parent().context("module has a parent")?)?;
        std::fs::write(&module, MODULE_TEXT)?;
        println!("wrote {}", module.display());
    }
    println!(
        "Commit both, then give each host that installs from the shell its entries:\n  \
         load(\"//{MODULE}\", \"apps\")\n  host(..., modules = [..., apps])"
    );
    Ok(())
}

fn title(edit: Edit, e: &Entry, paths: &Paths) -> String {
    let name = match e.source.as_str() {
        "flatpak" => crate::search::flathub_title(paths, &e.reference)
            .unwrap_or_else(|| e.reference.rsplit('.').next().unwrap_or(&e.reference).to_string()),
        _ => e.name.clone(),
    };
    match edit {
        Edit::Install => format!("Installing {name}"),
        Edit::Uninstall => format!("Uninstalling {name}"),
    }
}

fn change(
    paths: &Paths,
    spec_path: &Path,
    id: &str,
    host: Option<&str>,
    edit: Edit,
    detach: bool,
    op_id: Option<String>,
) -> Result<()> {
    let wanted = entry(id, &today())?;
    let host = host.map(str::to_string).unwrap_or_else(local_hostname);
    if detach {
        return self::detach(spec_path, &[edit.verb()], Some(id), &host);
    }
    let kind = match edit {
        Edit::Install => Kind::Install,
        Edit::Uninstall => Kind::Uninstall,
    };
    let mut op = Operation::new(kind, &wanted.id(), title(edit, &wanted, paths), Some(&host));
    if let Some(given) = op_id {
        op.id = given;
    }
    let op_id = op.id.clone();
    activity::run(op, || {
        let spec_dir = spec_path.parent().context("spec has no parent directory")?;
        let repo = Repo::find(spec_dir)?;
        let queue = paths.user_state_dir().join("queue");
        // Unique, so another apply's run never counts as covering this one.
        let request = fspec_core::queue::Request {
            spec_dir: spec_dir.to_path_buf(),
            rev: Some(format!("app:{op_id}")),
            scope: None,
        };
        fspec_core::queue::run(&queue, &host, &request, true, |_| {
            let mut tries = 0;
            let mut applied: Option<String> = None;
            let mut over_ssh: Option<String> = None;
            // The host the change landed on keeps the commit, so an apply from a checkout that
            // lacks it is refused there.
            let mark = |over_ssh: &Option<String>, commit: &str| -> Result<()> {
                let Some(alias) = over_ssh else { return record(paths, commit) };
                let out = LiveRunner.run(&fspec_remote::cmd::mark_apps(alias, commit))?;
                if out.status != 0 {
                    bail!(
                        "{host} has the change and {} is pushed, but recording it there failed, so an apply from \
                         an older checkout would undo it: {}",
                        &commit[..12],
                        out.stderr.trim()
                    );
                }
                Ok(())
            };
            loop {
                tries += 1;
                activity::phase(Phase::Resolving, None);
                let base = repo.base(&host)?;
                if tries == 1 {
                    repo.sync(&base)?;
                }
                let scratch = tempfile::tempdir()?;
                let spec_name = spec_path.file_name().context("spec has no file name")?;
                let made = prepare(paths, &repo, spec_name, &base, &host, &wanted, edit, scratch.path())?;
                if applied.is_none() {
                    activity::phase(Phase::Evaluating, None);
                    let pinned = Pinned::new(spec_path, Some(&made.commit))?;
                    let spec = fspec_eval::evaluate(&pinned.spec_path)?;
                    let h = spec.host(&host)?;
                    let this = local_hostname();
                    match remote_alias(h, &this, paths, false) {
                        Some(alias) => {
                            activity::phase(Phase::Remote, None);
                            activity::step(|s| s.item = Some(format!("{host} over ssh")));
                            pinned.remote(false, true)?.apply(h, &alias, Some(crate::scope_name(made.scope)))?;
                            over_ssh = Some(alias);
                        }
                        None => apply_local(paths, &pinned, h, Some(made.scope), true, false)?,
                    }
                    applied = Some(made.commit.clone());
                }
                activity::phase(Phase::Pushing, None);
                activity::step(|s| s.item = Some(format!("{}/{}", repo.remote, repo.branch)));
                match repo.push(&made.commit)? {
                    Pushed::Done => {
                        let _ = LiveRunner.run(&git(&repo.top).args(["update-ref", "-d", &pending_ref(&host)]));
                        mark(&over_ssh, &made.commit)?;
                        repo.catch_up(&made.commit);
                        println!("{} {}", made.message, &made.commit[..12]);
                        return Ok(());
                    }
                    Pushed::Rejected if tries < 4 => continue,
                    Pushed::Rejected => {
                        bail!("{}/{} kept moving; the change is applied but not pushed", repo.remote, repo.branch)
                    }
                    Pushed::Offline(why) => {
                        LiveRunner.check(&git(&repo.top).args(["update-ref", &pending_ref(&host), &made.commit]))?;
                        mark(&over_ssh, &made.commit)?;
                        eprintln!(
                            "fspec: pushing failed, so {} keeps the commit and the next `fspec app` pushes it: {why}",
                            pending_ref(&host)
                        );
                        println!("{} {}", made.message, &made.commit[..12]);
                        return Ok(());
                    }
                }
            }
        })
        .map(drop)
    })
}

/// What a `feat(apps): <verb> <name> on <host>` subject says.
pub fn subject(s: &str) -> Option<(Edit, &str, &str)> {
    let rest = s.strip_prefix("feat(apps): ")?;
    let (verb, rest) = rest.split_once(' ')?;
    let (name, host) = rest.rsplit_once(" on ")?;
    let edit = match verb {
        "install" => Edit::Install,
        "uninstall" => Edit::Uninstall,
        _ => return None,
    };
    Some((edit, name, host))
}

/// The inverse of the newest `fspec app` commit for `host` on the upstream branch, of the package
/// `id` names when given: the edit and the search id to run it with.
fn last_change(spec_dir: &Path, host: &str, id: Option<&str>) -> Result<(Edit, String)> {
    let repo = Repo::find(spec_dir)?;
    let base = repo.base(host)?;
    let want = id.map(|i| entry(i, "")).transpose()?;
    let log =
        LiveRunner.check(&git(&repo.top).args(["log", "--format=%H %s", &base, "--"]).arg(repo.path(APPS)))?.stdout;
    for line in log.lines() {
        let Some((commit, s)) = line.split_once(' ') else { continue };
        let Some((edit, name, on)) = subject(s) else { continue };
        if on != host || want.as_ref().is_some_and(|w| w.name != name) {
            continue;
        }
        // The entry as it stood where it was listed: after an install, before an uninstall.
        let at = match edit {
            Edit::Install => commit.to_string(),
            Edit::Uninstall => format!("{commit}^"),
        };
        let text = repo.show(&at, APPS)?.with_context(|| format!("{} has no {APPS}", &commit[..12]))?;
        let e = parse(&text)?
            .remove(host)
            .unwrap_or_default()
            .into_iter()
            .find(|e| e.name == name)
            .with_context(|| format!("{} names {name}, which its {APPS} does not list", &commit[..12]))?;
        // A name can move between sources; an id given names one of them.
        if want.as_ref().is_some_and(|w| w.id() != e.id()) {
            continue;
        }
        let inverse = match edit {
            Edit::Install => Edit::Uninstall,
            Edit::Uninstall => Edit::Install,
        };
        return Ok((inverse, e.id()));
    }
    match id {
        Some(id) => bail!("no `fspec app` change of {id} on {host} to undo"),
        None => bail!("no `fspec app` change on {host} to undo"),
    }
}

struct Made {
    commit: String,
    message: String,
    scope: ScopeArg,
}

/// The commit on `base` that makes the edit: `apps.star` rewritten and, for an install, the new
/// package resolved into `fspec.lock`. Evaluated before it is committed, so a host that does not
/// load the module is refused here.
#[allow(clippy::too_many_arguments)]
fn prepare(
    paths: &Paths,
    repo: &Repo,
    spec_name: &std::ffi::OsStr,
    base: &str,
    host: &str,
    e: &Entry,
    edit: Edit,
    scratch: &Path,
) -> Result<Made> {
    let text = repo.show(base, APPS)?.with_context(|| {
        format!(
            "{} has no {}; run `fspec app init`, commit what it writes and add `apps` to host {host}'s modules",
            &base[..12],
            repo.path(APPS)
        )
    })?;
    let apps = parse(&text)?;
    repo.unpack(base, scratch)?;
    let dir = scratch.join("tree").join(&repo.rel);
    let spec_file = dir.join(spec_name);
    let before = fspec_eval::evaluate(&spec_file)?;
    let declared: Vec<&Pkg> = pkgs(before.host(host)?).collect();
    let scope_of = |p: &Pkg| match Resource::Pkg(p.clone()).scope() {
        Scope::System => ScopeArg::System,
        Scope::User => ScopeArg::User,
    };
    let old_scope = declared.iter().find(|p| same(p, &e.source, &e.reference)).map(|p| scope_of(p));
    let new = edited(&apps, host, e, edit, &declared)?;
    std::fs::write(dir.join(APPS), render(&new)?)?;
    let mut files: Vec<(&str, PathBuf)> = vec![(APPS, dir.join(APPS))];
    let scope = match edit {
        Edit::Install => {
            let after = fspec_eval::evaluate(&spec_file)?;
            let p = pkgs(after.host(host)?).find(|p| p.name == e.name).cloned().with_context(|| {
                format!("host {host} does not load {MODULE}; add `apps` to its modules so its {APPS} entries reach it")
            })?;
            activity::step(|s| s.item = Some(format!("{} into fspec.lock", p.name)));
            resolve(paths, &dir, &spec_file, &p.name)?;
            files.push(("fspec.lock", dir.join("fspec.lock")));
            scope_of(&p)
        }
        Edit::Uninstall => old_scope.unwrap_or(ScopeArg::System),
    };
    activity::phase(Phase::Committing, None);
    activity::step(|s| s.item = Some("spec".into()));
    let message = format!("feat(apps): {} {} on {host}", edit.verb(), e.name);
    let refs: Vec<(&str, &Path)> = files.iter().map(|(f, p)| (*f, p.as_path())).collect();
    let commit = repo.commit(base, &refs, &message, scratch)?;
    Ok(Made { commit, message, scope })
}

/// `fspec update <name>` on the unpacked tree.
fn resolve(paths: &Paths, dir: &Path, spec_file: &Path, name: &str) -> Result<()> {
    let lock_path = dir.join("fspec.lock");
    let old = Lock::load(&lock_path)?;
    let (fetch, runner) = (fspec_core::fetch::LiveFetch::new(), LiveRunner);
    let spec = fspec_eval::evaluate_resolving(spec_file, &runner, &fetch, &old.loads, true)?;
    let sources =
        fspec_sources::Cx::new(&fetch, &runner).with_spec_dir(dir).with_catalog(crate::search::catalog(paths, true));
    let result = fspec_sources::update::update(&spec.hosts, &old, &[name.to_string()], &sources)?;
    if !result.errors.is_empty() {
        bail!("resolving {name}: {}", result.errors.join("; "));
    }
    let mut lock = result.lock;
    lock.loads = old.loads.clone();
    if lock != old {
        lock.save_over(&old, &lock_path)?;
    }
    Ok(())
}

/// Start the change in a session of its own and return once it is on the operation stream. The
/// child stays in this process's cgroup, and so in its login session, which is where polkit
/// looks for the agent that answers a pkexec prompt.
fn detach(spec_path: &Path, verb: &[&str], id: Option<&str>, host: &str) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let started = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis() as u64;
    let op = format!("{started:x}-{}", std::process::id());
    let log_dir = fspec_core::activity::dir();
    let mut c = std::process::Command::new(std::env::current_exe()?);
    c.arg("--spec").arg(spec_path).arg("app").args(verb).args(id).args(["--host", host, "--op", &op]);
    let what = [verb, id.as_slice()].concat().join(" ");
    c.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    // SAFETY: setsid is async-signal-safe and touches no memory of the parent.
    unsafe {
        c.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = c.spawn().context("starting fspec app in the background")?;
    let sock = log_dir.join(format!("{op}.sock"));
    let until = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while std::time::Instant::now() < until {
        if sock.exists() {
            println!("{}", serde_json::json!({ "op": op }));
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            bail!("fspec app {what} exited {status} before it published on the operation stream");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    bail!("fspec app {what} did not publish on the operation stream within 20s")
}

#[derive(Serialize, Debug, PartialEq, Eq)]
pub struct Owner {
    pub path: String,
    /// `<source>:<ref>`, what `fspec app uninstall` takes.
    pub id: Option<String>,
    /// The `pkg(...)` name when the spec declares it.
    pub pkg: Option<String>,
    pub source: Option<String>,
    /// Who installed it: `spec`, `flatpak` or `distro`.
    pub owner: Option<&'static str>,
    /// In this host's apps.star, so `fspec app uninstall` takes it out.
    pub removable: bool,
}

/// The desktop file a bare id names, from the XDG data directories in order.
fn desktop_file(paths: &Paths, id: &str) -> Option<PathBuf> {
    let file = if id.ends_with(".desktop") { id.to_string() } else { format!("{id}.desktop") };
    let home = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| paths.home.join(".local/share"));
    let dirs = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    std::iter::once(home)
        .chain([paths.home.join(".local/share/flatpak/exports/share"), "/var/lib/flatpak/exports/share".into()])
        .chain(dirs.split(':').filter(|d| !d.is_empty()).map(PathBuf::from))
        .map(|d| d.join("applications").join(&file))
        .find(|p| p.exists())
}

/// The native package behind `path`: a Flatpak export's app id, or the distro package that owns
/// the file.
fn native(runner: &dyn Runner, path: &Path, distro: &str) -> Result<Option<(String, String)>> {
    let s = path.display().to_string();
    if s.contains("/exports/share/applications/") {
        let stem = path.file_stem().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
        return Ok(Some(("flatpak".into(), stem)));
    }
    let (manager, cmd) = match distro_manager(distro) {
        Some(m @ ("zypper" | "dnf")) => (m, Cmd::new("rpm").args(["-qf", "--qf", "%{NAME}\\n"]).arg(&s)),
        Some("pacman") => ("pacman", Cmd::new("pacman").args(["-Qoq"]).arg(&s)),
        Some("apt") => ("apt", Cmd::new("dpkg").arg("-S").arg(&s)),
        _ => return Ok(None),
    };
    let out = runner.run(&cmd)?;
    let name = out.stdout.lines().next().unwrap_or("").trim();
    let name = if manager == "apt" { name.split(':').next().unwrap_or("") } else { name };
    Ok((out.status == 0 && !name.is_empty()).then(|| (manager.to_string(), name.to_string())))
}

/// `store/<dir>` for a file inside a store package under `root` (`/opt/fspec`, or the user store
/// `~/.local/share/fspec`), however many links lead there: the key a package's state record
/// keeps its store path under.
pub fn store_dir(target: &Path, root: &Path) -> Option<String> {
    let real = std::fs::canonicalize(target).ok()?;
    let root = std::fs::canonicalize(root).ok()?;
    let dir = real.strip_prefix(root.join("store")).ok()?.components().next()?;
    Some(format!("store/{}", dir.as_os_str().to_string_lossy()))
}

pub fn owner(paths: &Paths, spec_path: &Path, target: &str) -> Result<Owner> {
    let path = match target.contains('/') {
        true => PathBuf::from(target),
        false => {
            desktop_file(paths, target).with_context(|| format!("no desktop file {target} in the data directories"))?
        }
    };
    let host = local_hostname();
    let entries: Vec<Entry> = spec_path
        .parent()
        .and_then(|d| std::fs::read_to_string(d.join(APPS)).ok())
        .and_then(|t| parse(&t).ok())
        .and_then(|mut apps| apps.remove(&host))
        .unwrap_or_default();
    let mut o =
        Owner { path: path.display().to_string(), id: None, pkg: None, source: None, owner: None, removable: false };
    let states: Vec<(Scope, State)> = [Scope::System, Scope::User]
        .into_iter()
        .map(|scope| (scope, State::load(&state_dir(paths, scope).join("state.json")).unwrap_or_default()))
        .collect();
    let spec_pkg = |p: &Pkg, o: &mut Owner| {
        o.pkg = Some(p.name.clone());
        o.owner = Some("spec");
        if let Some(e) = entries.iter().find(|e| e.name == p.name && same(p, &e.source, &e.reference)) {
            o.id = Some(e.id());
            o.source = Some(e.source.clone());
            o.removable = true;
        }
    };
    let roots = [(Scope::System, paths.system("/opt/fspec")), (Scope::User, paths.home.join(".local/share/fspec"))];
    for (scope, root) in &roots {
        let Some(dir) = store_dir(&path, root) else { continue };
        let state = &states.iter().find(|(s, _)| s == scope).expect("both scopes loaded").1;
        let found = state.entries.values().find_map(|e| match &e.resource {
            Resource::Pkg(p) if e.record.get("store").and_then(|v| v.as_str()) == Some(dir.as_str()) => Some(p),
            _ => None,
        });
        if let Some(p) = found {
            o.id = Some(p.source.to_string());
            o.source = p.source.to_string().split(':').next().map(str::to_string);
            spec_pkg(p, &mut o);
        }
        return Ok(o);
    }
    let (_, distro) = fspec_core::facts::Facts::probe_local();
    let Some((source, reference)) = native(&LiveRunner, &path, &distro)? else { return Ok(o) };
    o.id = Some(format!("{source}:{reference}"));
    o.source = Some(source.clone());
    o.owner = Some(if source == "flatpak" { "flatpak" } else { "distro" });
    let declared = states.iter().find_map(|(_, state)| {
        state.entries.values().find_map(|e| match &e.resource {
            Resource::Pkg(p) if same(p, &source, &reference) => Some(p),
            _ => None,
        })
    });
    if let Some(p) = declared {
        spec_pkg(p, &mut o);
    }
    Ok(o)
}

/// Whether the system scope can be changed with no prompt, from a terminal, or only through
/// polkit.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Escalate {
    Sudo,
    Pkexec,
}

/// `sudo` when it needs no password or there is a terminal to ask in; with neither, `pkexec`,
/// whose prompt is the session's polkit agent's.
pub fn escalation(sudo_without_password: bool, terminal: bool, has_pkexec: bool) -> Escalate {
    match (sudo_without_password, terminal, has_pkexec) {
        (true, _, _) | (false, true, _) | (false, false, false) => Escalate::Sudo,
        (false, false, true) => Escalate::Pkexec,
    }
}

pub fn escalate() -> Escalate {
    let quiet = |c: &mut std::process::Command| {
        c.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    let sudo = quiet(std::process::Command::new("sudo").args(["-n", "true"]));
    let pkexec =
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).any(|d| d.join("pkexec").is_file());
    escalation(sudo, std::io::stdin().is_terminal(), pkexec)
}

/// `/usr/libexec/fspec/fspec` when a spec declares it and `/opt/fspec/current/bin/fspec` is the
/// running binary: the file the polkit action `dev.formalsnake.fspec.system-scope` names. pkexec
/// matches that path after resolving links, so neither the link nor the store path it reaches
/// could be named.
pub fn stable_exe(exe: PathBuf) -> PathBuf {
    match (std::fs::canonicalize(CURRENT_EXE), std::fs::canonicalize(&exe)) {
        (Ok(a), Ok(b)) if a == b && Path::new(PKEXEC_EXE).is_file() => PKEXEC_EXE.into(),
        _ => exe,
    }
}

const CURRENT_EXE: &str = "/opt/fspec/current/bin/fspec";
pub const PKEXEC_EXE: &str = "/usr/libexec/fspec/fspec";

/// What a pkexec exit status means, by pkexec(1).
pub fn pkexec_failure(status: i32) -> Option<&'static str> {
    match status {
        126 => Some("authorisation cancelled; nothing in the system scope changed"),
        127 => Some("not authorised to change system software (no polkit agent answered, or the password was refused)"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(id: &str) -> Entry {
        entry(id, "2026-10-30").unwrap()
    }

    fn flatpak_pkg(name: &str, id: Option<&str>) -> Pkg {
        serde_json::from_value(serde_json::json!({
            "name": name, "source": {"kind": "flatpak", "id": id, "remote": "flathub"}
        }))
        .unwrap()
    }

    #[test]
    fn a_search_id_becomes_an_entry_named_as_the_spec_would_name_it() {
        assert_eq!(e("flatpak:org.mozilla.firefox").name, "org.mozilla.firefox");
        assert_eq!(e("npm:@scope/cowsay").name, "cowsay");
        assert_eq!(e("zypper:btop").name, "btop");
        assert_eq!(e("zypper:btop").id(), "zypper:btop");
        assert!(entry("btop", "d").unwrap_err().to_string().contains("not a search id"));
        assert!(entry("flatpak:firefox", "d").is_err());
        assert!(entry("nix:hello", "d").is_err());
        assert!(entry("zypper:a b", "d").is_err());
    }

    #[test]
    fn apps_star_reads_back_what_it_writes_and_is_valid_starlark() {
        let mut apps = Apps::new();
        apps.insert("e1504g".into(), vec![e("flatpak:org.mozilla.firefox"), e("zypper:btop")]);
        let text = render(&apps).unwrap();
        assert!(text.starts_with("# Written by `fspec app install`"));
        assert_eq!(parse(&text).unwrap(), apps);
        assert!(parse("APPS = {}\n").unwrap().is_empty());
        assert!(parse("x = 1").is_err());
    }

    #[test]
    fn the_module_turns_entries_into_packages_for_its_own_host_only() {
        let dir = tempfile::tempdir().unwrap();
        let mut apps = Apps::new();
        apps.insert("a".into(), vec![e("flatpak:org.mozilla.firefox"), e("zypper:btop"), e("npm:cowsay")]);
        apps.insert("b".into(), vec![e("cargo:xh")]);
        std::fs::write(dir.path().join(APPS), render(&apps).unwrap()).unwrap();
        std::fs::create_dir(dir.path().join("modules")).unwrap();
        std::fs::write(dir.path().join(MODULE), MODULE_TEXT).unwrap();
        std::fs::write(
            dir.path().join("spec.star"),
            "load(\"//modules/apps.star\", \"apps\")\n\
             hosts = [host(name = \"a\", platform = \"x86_64-linux\", system = tumbleweed(), modules = [apps]), \
             host(name = \"b\", platform = \"x86_64-linux\", system = tumbleweed(), modules = [apps]), \
             host(name = \"c\", platform = \"x86_64-linux\", system = tumbleweed())]\n",
        )
        .unwrap();
        let spec = fspec_eval::evaluate(&dir.path().join("spec.star")).unwrap();
        let names = |h: &str| {
            pkgs(spec.host(h).unwrap())
                .map(|p| (p.name.clone(), p.source.to_string()))
                .filter(|(n, _)| n != "fspec")
                .collect::<Vec<_>>()
        };
        let a = names("a");
        assert_eq!(a.len(), 3, "{a:?}");
        assert!(a.iter().any(|(n, s)| n == "org.mozilla.firefox" && s.starts_with("flatpak")), "{a:?}");
        let a_pkgs: Vec<Pkg> = pkgs(spec.host("a").unwrap()).cloned().collect();
        assert!(a_pkgs.iter().any(|p| same(p, "zypper", "btop")));
        assert!(a_pkgs.iter().any(|p| same(p, "npm", "cowsay")));
        assert!(a_pkgs.iter().any(|p| same(p, "flatpak", "org.mozilla.firefox")));
        assert_eq!(names("b").len(), 1);
        assert!(names("c").is_empty());
    }

    #[test]
    fn an_install_is_refused_where_the_spec_already_has_it_and_an_uninstall_outside_apps_star() {
        let parsec = flatpak_pkg("parsec", Some("com.parsecgaming.parsec"));
        let declared = vec![&parsec];
        let apps = Apps::new();
        let err = edited(&apps, "h", &e("flatpak:com.parsecgaming.parsec"), Edit::Install, &declared).unwrap_err();
        assert!(err.to_string().contains("already declares parsec"), "{err}");
        let err = edited(&apps, "h", &e("flatpak:com.parsecgaming.parsec"), Edit::Uninstall, &declared).unwrap_err();
        assert!(err.to_string().contains("outside apps.star"), "{err}");
        let added = edited(&apps, "h", &e("zypper:btop"), Edit::Install, &declared).unwrap();
        assert_eq!(added["h"].len(), 1);
        let err = edited(&added, "h", &e("zypper:btop"), Edit::Install, &declared).unwrap_err();
        assert!(err.to_string().contains("already in h's apps.star"));
        let removed = edited(&added, "h", &e("zypper:btop"), Edit::Uninstall, &declared).unwrap();
        assert!(removed.is_empty(), "an emptied host leaves no key: {removed:?}");
        assert!(edited(&removed, "h", &e("zypper:btop"), Edit::Uninstall, &declared).is_err());
    }

    #[test]
    fn an_app_commit_subject_names_its_change() {
        assert_eq!(
            subject("feat(apps): install org.mozilla.firefox on e1504g"),
            Some((Edit::Install, "org.mozilla.firefox", "e1504g"))
        );
        assert_eq!(subject("feat(apps): uninstall figlet on g815"), Some((Edit::Uninstall, "figlet", "g815")));
        assert_eq!(subject("feat(apps): the apps module installs from the shell write"), None);
        assert_eq!(subject("fix(hypr): on g815"), None);
    }

    #[test]
    fn a_file_reached_through_links_into_the_store_names_its_store_dir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("opt/fspec");
        let app = root.join("store/f28c64f0-thing-1.0/share/applications");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(app.join("thing.desktop"), "").unwrap();
        let link = dir.path().join("thing.desktop");
        std::os::unix::fs::symlink(app.join("thing.desktop"), &link).unwrap();
        assert_eq!(store_dir(&link, &root).as_deref(), Some("store/f28c64f0-thing-1.0"));
        assert_eq!(store_dir(&dir.path().join("missing"), &root), None);
        std::fs::write(dir.path().join("plain.desktop"), "").unwrap();
        assert_eq!(store_dir(&dir.path().join("plain.desktop"), &root), None);
    }

    #[test]
    fn days_are_civil_dates() {
        assert_eq!(day(0), "1970-01-01");
        assert_eq!(day(1_793_318_400), "2026-10-30");
        assert_eq!(day(951_782_400), "2000-02-29");
    }

    #[test]
    fn what_a_host_can_install_follows_its_platform_and_distro() {
        assert!(installable("flatpak", "x86_64-linux", "opensuse-tumbleweed"));
        assert!(installable("zypper", "x86_64-linux", "opensuse-tumbleweed"));
        assert!(!installable("pacman", "x86_64-linux", "opensuse-tumbleweed"));
        assert!(!installable("brew", "x86_64-linux", "opensuse-tumbleweed"));
        assert!(installable("cask", "aarch64-darwin", "macos"));
        assert!(!installable("flatpak", "aarch64-darwin", "macos"));
    }

    #[test]
    fn the_system_scope_asks_polkit_only_with_no_terminal_and_a_sudo_that_wants_a_password() {
        assert_eq!(escalation(true, false, true), Escalate::Sudo);
        assert_eq!(escalation(false, true, true), Escalate::Sudo);
        assert_eq!(escalation(false, false, true), Escalate::Pkexec);
        assert_eq!(escalation(false, false, false), Escalate::Sudo);
        assert!(pkexec_failure(126).unwrap().starts_with("authorisation cancelled"));
        assert!(pkexec_failure(1).is_none());
    }

    /// A real repository in a temp dir: the edit is committed without touching the checkout's
    /// index or its uncommitted files, and the branch is fast-forwarded once pushed.
    #[test]
    fn a_commit_is_made_on_the_upstream_tip_through_its_own_index() {
        let dir = tempfile::tempdir().unwrap();
        let sh = |cwd: &Path, args: &[&str]| {
            let out = std::process::Command::new("git").current_dir(cwd).args(args).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };
        let origin = dir.path().join("origin.git");
        let work = dir.path().join("work");
        sh(dir.path(), &["init", "-q", "--bare", "-b", "main", origin.to_str().unwrap()]);
        sh(dir.path(), &["clone", "-q", origin.to_str().unwrap(), work.to_str().unwrap()]);
        // Commits take the identity git already has; a machine with none skips the test.
        let who = std::process::Command::new("git").args(["config", "user.email"]).output().unwrap();
        if who.stdout.is_empty() {
            eprintln!("skipped: git has no user.email here");
            return;
        }
        sh(&work, &["config", "commit.gpgsign", "false"]);
        std::fs::create_dir(work.join("spec")).unwrap();
        std::fs::write(work.join("spec/apps.star"), render(&Apps::new()).unwrap()).unwrap();
        std::fs::write(work.join("spec/spec.star"), "hosts = []\n").unwrap();
        sh(&work, &["add", "."]);
        sh(&work, &["commit", "-q", "-m", "init"]);
        sh(&work, &["push", "-q", "-u", "origin", "main"]);
        std::fs::write(work.join("spec/spec.star"), "hosts = [] # someone's edit\n").unwrap();

        let repo = Repo::find(&work.join("spec")).unwrap();
        assert_eq!((repo.rel.as_str(), repo.remote.as_str(), repo.branch.as_str()), ("spec", "origin", "main"));
        let base = repo.base("h").unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let new = scratch.path().join("apps.star");
        let mut apps = Apps::new();
        apps.insert("h".into(), vec![e("zypper:btop")]);
        std::fs::write(&new, render(&apps).unwrap()).unwrap();
        let commit = repo.commit(&base, &[(APPS, &new)], "feat(apps): install btop on h", scratch.path()).unwrap();
        assert_eq!(sh(&work, &["rev-parse", "HEAD"]), base, "no ref moved");
        assert_eq!(parse(&repo.show(&commit, APPS).unwrap().unwrap()).unwrap(), apps);
        assert_eq!(sh(&work, &["diff", "--cached", "--name-only"]), "", "the checkout's index is untouched");
        assert!(matches!(repo.push(&commit).unwrap(), Pushed::Done));
        repo.catch_up(&commit);
        assert_eq!(sh(&work, &["rev-parse", "HEAD"]), commit);
        assert_eq!(sh(&work, &["status", "--porcelain"]), "M spec/spec.star", "the edit is still there");

        let stale = repo.commit(&base, &[(APPS, &new)], "again", scratch.path()).unwrap();
        assert!(matches!(repo.push(&stale).unwrap(), Pushed::Rejected));

        sh(&work, &["reset", "-q", "--keep", &base]);
        repo.sync(&commit).unwrap();
        assert_eq!(sh(&work, &["rev-parse", "HEAD"]), commit, "a checkout behind is fast-forwarded");
        sh(&work, &["commit", "-q", "-am", "local only"]);
        let err = repo.sync(&commit).unwrap_err().to_string();
        assert!(err.contains("has commits origin/main does not"), "{err}");
    }
}
