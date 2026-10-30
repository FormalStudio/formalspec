//! `fspec info`: one search hit's details for the launcher's Install pane, read where the catalog
//! keeps them and never invented. A Flathub app's come off its own AppStream component and its
//! sizes off the remote's cached summary (`flatpak remote-info --cached`), a zypper package's off
//! `zypper info`, which carries no licence, developer or screenshot. Any other source answers its
//! id and source alone.

use anyhow::{Result, bail};
use fspec_core::Paths;
use fspec_core::runner::{Cmd, LiveRunner, Runner};
use fspec_sources::catalog;
use serde::Serialize;

#[derive(Serialize, Debug, Default, PartialEq, Eq)]
pub struct Info {
    pub id: String,
    pub source: String,
    pub name: Option<String>,
    pub version: Option<String>,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub license: Option<String>,
    pub developer: Option<String>,
    /// The size a download takes and what it takes once installed, as the tool words them.
    pub download: Option<String>,
    pub installed: Option<String>,
    pub screenshot: Option<String>,
    pub icon: Option<String>,
}

pub fn run(paths: &Paths, id: &str, json: bool) -> Result<()> {
    let Some((source, name)) = id.split_once(':') else { bail!("`{id}` is not a search id (<source>:<name>)") };
    let mut info = Info { id: id.to_string(), source: source.to_string(), ..Info::default() };
    match source {
        "flatpak" => flatpak(paths, name, &mut info, &LiveRunner),
        "zypper" => zypper(name, &mut info, &LiveRunner),
        _ => {}
    }
    if json {
        println!("{}", serde_json::to_string(&info)?);
    } else {
        for (k, v) in [
            ("name", &info.name),
            ("version", &info.version),
            ("summary", &info.summary),
            ("license", &info.license),
            ("developer", &info.developer),
            ("download", &info.download),
            ("installed", &info.installed),
            ("screenshot", &info.screenshot),
        ] {
            if let Some(v) = v {
                println!("{k:>10}: {v}");
            }
        }
        if let Some(d) = &info.description {
            println!("\n{d}");
        }
    }
    Ok(())
}

fn flatpak(paths: &Paths, app: &str, info: &mut Info, runner: &dyn Runner) {
    let cat = crate::search::catalog(paths, false);
    if let Some(d) = fspec_sources::appstream::newest(&cat.appstream)
        .and_then(|active| fspec_sources::appstream::detail(&active, app).ok().flatten())
    {
        info.name = d.name;
        info.summary = d.summary;
        info.description = d.description;
        info.version = d.version;
        info.license = d.license;
        info.developer = d.developer;
        info.screenshot = d.screenshot;
        info.icon = d.icon;
    }
    // The cached summary carries both sizes; `remote-info --cached` leaves them out.
    let cmd = Cmd::new("flatpak")
        .args(["--user", "remote-ls", "--cached", "--app", "--columns=application,download-size,installed-size", "flathub"])
        .env("FLATPAK_USER_DIR", catalog::fspec_flatpak(&paths.home).display().to_string())
        .env("LC_ALL", "C");
    if let Ok(out) = runner.run(&cmd).filter_status()
        && let Some((download, installed)) = sizes(&out, app)
    {
        info.download = Some(download);
        info.installed = Some(installed);
    }
}

/// `remote-ls`'s `<id>\t<download>\t<installed>` line for `app`.
fn sizes(out: &str, app: &str) -> Option<(String, String)> {
    let line = out.lines().find(|l| l.split('\t').next() == Some(app))?;
    let mut cols = line.split('\t').skip(1).map(|c| c.trim().to_string());
    Some((cols.next()?, cols.next()?))
}

fn zypper(package: &str, info: &mut Info, runner: &dyn Runner) {
    let cmd = Cmd::new("zypper").args(["--no-refresh", "--non-interactive", "--quiet", "info", package]).env("LC_ALL", "C");
    let Ok(out) = runner.run(&cmd).filter_status() else { return };
    parse_zypper(&out, info);
}

/// `zypper info`'s `Key : value` lines, then the description, which runs to the end indented under
/// its key.
fn parse_zypper(out: &str, info: &mut Info) {
    let f = fields(out);
    let get = |k: &str| f.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone()).filter(|v| !v.is_empty());
    info.name = get("Name");
    info.version = get("Version");
    info.summary = get("Summary");
    info.installed = get("Installed Size");
    let Some(at) = out.lines().position(|l| l.trim_start().starts_with("Description")) else { return };
    let mut paragraphs: Vec<String> = vec![];
    let mut current = String::new();
    let head = out.lines().nth(at).and_then(|l| l.split_once(':')).map(|(_, v)| v.trim().to_string()).unwrap_or_default();
    for line in std::iter::once(head.as_str()).chain(out.lines().skip(at + 1)) {
        let line = line.trim();
        if line.is_empty() {
            if !current.is_empty() {
                paragraphs.push(std::mem::take(&mut current));
            }
        } else {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(line);
        }
    }
    if !current.is_empty() {
        paragraphs.push(current);
    }
    info.description = (!paragraphs.is_empty()).then(|| paragraphs.join("\n"));
}

/// `Key : value` lines, keys trimmed.
fn fields(out: &str) -> Vec<(String, String)> {
    out.lines()
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .filter(|(k, _)| !k.is_empty() && !k.contains("  "))
        .collect()
}

trait Status {
    fn filter_status(self) -> Result<String>;
}

impl Status for Result<fspec_core::runner::Output> {
    fn filter_status(self) -> Result<String> {
        let out = self?;
        if out.status != 0 {
            bail!("exit {}", out.status);
        }
        Ok(out.stdout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_ls_gives_the_apps_own_two_sizes() {
        let out = "org.mozilla.firefox.Locale\t1 MB\t2 MB\norg.mozilla.firefox\t129.5 MB\t345.2 MB\n";
        assert_eq!(sizes(out, "org.mozilla.firefox"), Some(("129.5 MB".into(), "345.2 MB".into())));
        assert_eq!(sizes(out, "org.absent"), None);
    }

    #[test]
    fn zypper_info_reads_version_summary_size_and_the_whole_description() {
        let out = "\nInformation for package btop:\n-----------------------------\nRepository     : Main Repository (OSS)\nName           : btop\nVersion        : 1.4.0-1.2\nArch           : x86_64\nVendor         : openSUSE\nInstalled Size : 1.1 MiB\nInstalled      : No\nStatus         : not installed\nSource package : btop-1.4.0-1.2.src\nUpstream URL   : https://github.com/aristocratos/btop\nSummary        : Usage and stats for processor, memory, disks, network and processes\nDescription    : \n    Resource monitor that shows usage and stats for processor,\n    memory, disks, network and processes.\n\n    C++ version and continuation of bashtop and bpytop.\n";
        let mut info = Info::default();
        parse_zypper(out, &mut info);
        assert_eq!(info.name.as_deref(), Some("btop"));
        assert_eq!(info.version.as_deref(), Some("1.4.0-1.2"));
        assert_eq!(info.installed.as_deref(), Some("1.1 MiB"));
        assert_eq!(info.summary.as_deref(), Some("Usage and stats for processor, memory, disks, network and processes"));
        assert_eq!(
            info.description.as_deref(),
            Some("Resource monitor that shows usage and stats for processor, memory, disks, network and processes.\nC++ version and continuation of bashtop and bpytop.")
        );
        assert_eq!((info.license, info.developer, info.screenshot), (None, None, None));
    }
}
