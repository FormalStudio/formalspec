//! The spec repo the installer writes into the new system.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::model::{Choices, Draft, Preset, SystemChoice};

const MINIMAL: &str = include_str!("../presets/minimal.star");
const DESKTOP: &str = include_str!("../presets/desktop.star");
const GAMING: &str = include_str!("../presets/gaming.star");
const SERVER: &str = include_str!("../presets/server.star");
const FLATHUB: &str = include_str!("../presets/files/flathub.flatpakrepo");

const HEADER: &str = "# This machine's spec. `fspec plan` shows what a change would do, `fspec apply` makes it so.\n";

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The module files a preset loads, itself last.
fn modules(preset: Preset) -> Vec<(&'static str, &'static str)> {
    let minimal = ("minimal", MINIMAL);
    match preset {
        Preset::Minimal => vec![minimal],
        Preset::Desktop => vec![minimal, ("desktop", DESKTOP)],
        Preset::Gaming => vec![minimal, ("desktop", DESKTOP), ("gaming", GAMING)],
        Preset::Server => vec![minimal, ("server", SERVER)],
    }
}

fn admin_group(distro: &str) -> &'static str {
    match distro {
        "debian" | "ubuntu" => "sudo",
        _ => "wheel",
    }
}

/// `spec.star` for `draft`, with the parts not chosen yet left out. A restore shows the cloned one.
pub fn preview(d: &Draft) -> String {
    let mut out = String::from(HEADER);
    if let Some(SystemChoice::Restore { checkout, .. }) = &d.system {
        return std::fs::read_to_string(checkout.join("spec.star")).unwrap_or(out);
    }
    let fresh = match &d.system {
        Some(SystemChoice::Fresh { distro, preset, desktop }) => Some((*distro, preset.id(), desktop.map(|d| d.id()))),
        _ => None,
    };
    if let Some((_, preset, _)) = fresh {
        out.push_str(&format!("load(\"//modules/{preset}.star\", \"modules\", \"system\")\n"));
    }
    out.push_str("\nhosts = [\n    host(\n");
    if let Some(a) = &d.account {
        out.push_str(&format!("        name = {},\n", quote(&a.hostname)));
    }
    out.push_str("        platform = \"x86_64-linux\",\n");
    if let Some(l) = &d.locale {
        out.push_str(&format!(
            "        locale = {},\n        keymap = {},\n        timezone = {},\n",
            quote(&l.locale),
            quote(&l.keymap),
            quote(&l.timezone)
        ));
    }
    if let Some((distro, _, desktop)) = fresh {
        let args = desktop.map(|d| format!(", desktop = \"{d}\"")).unwrap_or_default();
        let distro = quote(distro);
        out.push_str(&format!(
            "        system = system({distro}{args}),\n        modules = modules({distro}{args}),\n"
        ));
    }
    if let Some(a) = &d.account {
        let group = fresh.map_or("wheel", |(distro, _, _)| admin_group(distro));
        // useradd's own default is /bin/sh on Debian, Ubuntu and Void.
        out.push_str(&format!(
            "        users = [user({}, uid = 1000, groups = [\"{group}\"], shell = \"bash\")],\n",
            quote(&a.name)
        ));
    }
    out.push_str("        protect = [\"/home\"],\n    ),\n]\n");
    out
}

/// Every file of the repo for a fresh system, relative to its root. Empty for a restore, which
/// installs the repo the user already has.
pub fn files(c: &Choices) -> Vec<(PathBuf, String)> {
    let SystemChoice::Fresh { preset, .. } = &c.system else { return Vec::new() };
    let draft = Draft {
        locale: Some(c.locale.clone()),
        disk: Some(c.disk.clone()),
        system: Some(c.system.clone()),
        account: Some(c.account.clone()),
        secure_boot: c.secure_boot,
        keys: Vec::new(),
    };
    let mut out = vec![(PathBuf::from("spec.star"), preview(&draft))];
    for (name, source) in modules(*preset) {
        out.push((PathBuf::from(format!("modules/{name}.star")), source.to_string()));
    }
    if matches!(preset, Preset::Desktop | Preset::Gaming) {
        out.push((PathBuf::from("files/flathub.flatpakrepo"), FLATHUB.to_string()));
    }
    out.push((PathBuf::from(".gitignore"), "*~\n*.swp\n.DS_Store\n".to_string()));
    out
}

/// Write the repo into `dir` and commit it.
pub fn write_repo(dir: &Path, c: &Choices) -> Result<()> {
    let files = files(c);
    if files.is_empty() {
        bail!("a restored system installs the repo it was cloned from; there is nothing to write");
    }
    for (path, text) in files {
        let full = dir.join(&path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&full, text).with_context(|| format!("writing {}", full.display()))?;
    }
    git(dir, &["init", "-q", "-b", "main"])?;
    git(dir, &["add", "-A"])?;
    // A brand-new repo for the new machine, so the identity is set on this one command.
    git(
        dir,
        &[
            "-c",
            "user.name=FormalSpec installer",
            "-c",
            "user.email=installer@formalspec.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            "initial spec from the installer",
        ],
    )
}

fn git(dir: &Path, args: &[&str]) -> Result<()> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().context("running git")?;
    if !out.status.success() {
        bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
