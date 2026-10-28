//! App data of removed packages. `apply` moves it into `<state dir>/trash/<generation>/` after
//! the package is gone, journaled like any other mutation, and brings it back when the package
//! is added again (which is what `rollback` does).

use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::apply::{Cx, state_dir};
use crate::journal::{Journal, Undo, move_any};
use crate::paths::Paths;
use crate::resource::{Pkg, Scope, Source};
use crate::runner::Cmd;

const MANIFEST: &str = "manifest.json";

/// One thing in the trash. `stored` is relative to the generation directory.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Item {
    /// `None` for an orphan deleted by hand, which no package brings back.
    pub pkg: Option<String>,
    /// Where it lived, as a spec path.
    pub from: String,
    pub stored: String,
}

#[derive(Serialize, Deserialize, Default)]
struct Manifest {
    #[serde(default)]
    items: Vec<Item>,
}

impl Manifest {
    fn load(dir: &Path) -> Result<Manifest> {
        let path = dir.join(MANIFEST);
        match fs::read_to_string(&path) {
            Ok(t) => serde_json::from_str(&t).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Manifest::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    fn save(&self, dir: &Path) -> Result<()> {
        fs::create_dir_all(dir)?;
        let path = dir.join(MANIFEST);
        fs::write(&path, serde_json::to_vec_pretty(self)?).with_context(|| format!("writing {}", path.display()))
    }
}

/// Where trashed paths for `scope` live.
pub fn trash_root(paths: &Paths, scope: Scope) -> PathBuf {
    state_dir(paths, scope).join("trash")
}

/// Home paths trash into the user's state, everything else into the system's. The system scope
/// reaches into a home for Flatpak data, so its directories there carry a `system-` prefix and
/// never collide with the user scope's own generation numbers.
fn dest_dir(cx: &Cx, path: &Path) -> PathBuf {
    let loc = if path.starts_with(&cx.paths.home) { Scope::User } else { Scope::System };
    let name =
        if loc == cx.scope { cx.generation.to_string() } else { format!("{}-{}", scope_name(cx.scope), cx.generation) };
    trash_root(cx.paths, loc).join(name)
}

fn scope_name(scope: Scope) -> &'static str {
    match scope {
        Scope::System => "system",
        Scope::User => "user",
    }
}

/// The trash directories `scope` wrote, newest generation first.
fn scope_dirs(paths: &Paths, scope: Scope) -> Vec<(u64, PathBuf)> {
    let mut dirs = Vec::new();
    for loc in [Scope::System, Scope::User] {
        let prefix = (loc != scope).then(|| format!("{}-", scope_name(scope)));
        let Ok(read) = fs::read_dir(trash_root(paths, loc)) else { continue };
        for entry in read.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let number = match &prefix {
                Some(p) => name.strip_prefix(p.as_str()),
                None => Some(name.as_str()),
            };
            if let Some(n) = number.and_then(|n| n.parse::<u64>().ok()) {
                dirs.push((n, entry.path()));
            }
        }
    }
    dirs.sort_by_key(|d| std::cmp::Reverse(d.0));
    dirs
}

/// Move `paths` into the trash for generation `cx.generation`, remembering them under `pkg`.
pub fn trash_paths(cx: &Cx, journal: &mut Journal, pkg: Option<&str>, paths: &[PathBuf]) -> Result<()> {
    for path in paths {
        cx.check_path(path)?;
        let dir = dest_dir(cx, path);
        let mut manifest = Manifest::load(&dir)?;
        let base = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut n = manifest.items.len();
        let stored = loop {
            let candidate = format!("{}/{n}-{base}", pkg.unwrap_or("orphans"));
            if !dir.join(&candidate).exists() {
                break candidate;
            }
            n += 1;
        };
        journal.backup(&dir.join(MANIFEST))?;
        journal.record(Undo::Move { from: dir.join(&stored), to: path.clone() })?;
        move_any(path, &dir.join(&stored)).with_context(|| format!("moving {} to the trash", path.display()))?;
        manifest.items.push(Item { pkg: pkg.map(Into::into), from: cx.paths.to_spec(path), stored });
        manifest.save(&dir)?;
        give_home_trash(cx, &dir)?;
    }
    Ok(())
}

/// The system scope runs as root, so the home trash directories it makes would be root's, and
/// the user scope could no longer trash anything into them.
fn give_home_trash(cx: &Cx, dir: &Path) -> Result<()> {
    if cx.scope != Scope::System || !dir.starts_with(&cx.paths.home) {
        return Ok(());
    }
    let home = fs::metadata(&cx.paths.home).with_context(|| format!("reading {}", cx.paths.home.display()))?;
    let owner = format!("{}:{}", home.uid(), home.gid());
    let root = trash_root(cx.paths, Scope::User);
    cx.runner.check(&Cmd::new("chown").args(["-R", &owner]).arg(root.display().to_string()))?;
    Ok(())
}

/// Put back what the newest trashed generation holds for `pkg`. A path that exists again stays
/// in the trash.
pub fn restore_pkg(cx: &Cx, journal: &mut Journal, pkg: &str) -> Result<()> {
    let mut found: Option<u64> = None;
    for (n, dir) in scope_dirs(cx.paths, cx.scope) {
        if found.is_some_and(|f| f != n) {
            break;
        }
        let mut manifest = Manifest::load(&dir)?;
        let (mine, rest): (Vec<Item>, Vec<Item>) =
            std::mem::take(&mut manifest.items).into_iter().partition(|i| i.pkg.as_deref() == Some(pkg));
        manifest.items = rest;
        if mine.is_empty() {
            continue;
        }
        found = Some(n);
        journal.backup(&dir.join(MANIFEST))?;
        for item in mine {
            let target = cx.resolve(&item.from)?;
            let stored = dir.join(&item.stored);
            if fs::symlink_metadata(&target).is_ok() || fs::symlink_metadata(&stored).is_err() {
                manifest.items.push(item);
                continue;
            }
            cx.check_path(&target)?;
            journal.record(Undo::Move { from: target.clone(), to: stored.clone() })?;
            move_any(&stored, &target).with_context(|| format!("restoring {}", target.display()))?;
        }
        manifest.save(&dir)?;
    }
    Ok(())
}

/// Every path `pkg` would lose on removal that exists now, outermost first and without
/// duplicates: declared, known from its source, attributed.
pub fn data_paths(cx: &Cx, pkg: &Pkg, claims: &BTreeMap<String, Vec<String>>) -> Result<Vec<PathBuf>> {
    let specs =
        pkg.data.iter().cloned().chain(known_data(pkg)).chain(claims.get(&pkg.name).into_iter().flatten().cloned());
    let mut found: Vec<PathBuf> = Vec::new();
    // Two spellings of one path (`Steam` and `steam` on a case-insensitive disk) count once.
    let mut real: Vec<PathBuf> = Vec::new();
    for spec in specs {
        let path = cx.resolve(&spec)?;
        let Ok(canonical) = fs::canonicalize(path.parent().unwrap_or(&path)) else { continue };
        let Some(name) = path.file_name() else { continue };
        let canonical = fs::canonicalize(canonical.join(name)).unwrap_or_else(|_| canonical.join(name));
        if fs::symlink_metadata(&path).is_ok() && !real.contains(&canonical) {
            real.push(canonical);
            found.push(path);
        }
    }
    found.sort();
    let all = found.clone();
    found.retain(|p| !all.iter().any(|other| other != p && p.starts_with(other)));
    Ok(found)
}

/// Data the source or a built-in table names without the spec saying so.
pub fn known_data(pkg: &Pkg) -> Vec<String> {
    if let Source::Flatpak { id, .. } = &pkg.source {
        return vec![format!("~/.var/app/{}", id.as_deref().unwrap_or(&pkg.name))];
    }
    let name = pkg.name.to_lowercase();
    let dirs: &[&str] = match name.as_str() {
        "neovim" | "nvim" => &["nvim"],
        "helix" | "hx" => &["helix"],
        "zed" => &["zed"],
        "code" | "vscode" | "visual-studio-code" => &["Code"],
        "steam" => &["Steam"],
        "obs-studio" | "obs" => &["obs-studio"],
        "discord" => &["discord"],
        "spotify" => &["spotify"],
        "firefox" => &["mozilla"],
        "chromium" => &["chromium"],
        "lazygit" => &["lazygit"],
        "gh" => &["gh"],
        "btop" => &["btop"],
        "yazi" => &["yazi"],
        "kitty" => &["kitty"],
        "alacritty" => &["alacritty"],
        "wezterm" => &["wezterm"],
        "ghostty" => &["ghostty"],
        _ => &[],
    };
    dirs.iter()
        .flat_map(|d| [".config", ".local/share", ".local/state", ".cache"].map(|base| format!("~/{base}/{d}")))
        .collect()
}
