//! The backend on the installer ISO, reading and changing the machine it booted on.

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use fspec_core::Paths;
use fspec_core::resource::System;
use fspec_core::runner::{Cmd, LiveRunner, Runner};
use serde_json::Value;

use crate::backend::Backend;
use crate::model::{Choices, DiskInfo, Draft, Firmware, GitAuth, HostSummary, Progress, SystemChoice};
use crate::{install, keys, spec};

pub const LSBLK: [&str; 6] =
    ["-J", "-b", "-p", "-o", "NAME,TYPE,SIZE,MODEL,TRAN,RM,FSTYPE,LABEL,PARTLABEL,MOUNTPOINTS", "-e7"];

pub fn disks(runner: &dyn Runner) -> Result<Vec<DiskInfo>> {
    let out = runner.check(&Cmd::new("lsblk").args(LSBLK))?;
    let mut disks = parse_disks(&out.stdout)?;
    for d in disks.iter_mut().filter(|d| !d.live_medium) {
        d.windows = windows(runner, &d.path).unwrap_or_default();
    }
    Ok(disks)
}

/// Windows on `disk`, with how far ntfsresize could shrink it. ntfsresize only reads here.
pub fn windows(runner: &dyn Runner, disk: &str) -> Result<Option<fspec_switch::WindowsDisk>> {
    let sfdisk = runner.check(&Cmd::new("sfdisk").args(["--json", disk]))?.stdout;
    let lsblk = runner.check(&Cmd::new("lsblk").args(["-J", "-b", "-p", "-o", "NAME,FSTYPE,UUID", disk]))?.stdout;
    let Some(mut w) = fspec_switch::read_windows(&sfdisk, &lsblk, Err(String::new()))? else { return Ok(None) };
    if !w.bitlocker() {
        let out = runner.run(&fspec_switch::ntfs_info(&w.windows.dev))?;
        w.shrink_min = fspec_switch::shrink_min(out.status, &format!("{}{}", out.stdout, out.stderr));
    }
    Ok(Some(w))
}

pub(crate) fn parse_disks(json: &str) -> Result<Vec<DiskInfo>> {
    let v: Value = serde_json::from_str(json).context("reading lsblk's output")?;
    let mut out = Vec::new();
    for d in v["blockdevices"].as_array().into_iter().flatten() {
        if d["type"].as_str() != Some("disk") {
            continue;
        }
        let str_of = |v: &Value, k: &str| v[k].as_str().unwrap_or_default().trim().to_string();
        let mut contents = Vec::new();
        let mut live = false;
        let mut each = |p: &Value| {
            let mounts = p["mountpoints"].as_array().into_iter().flatten().filter_map(Value::as_str);
            live |= mounts.into_iter().any(|m| m.starts_with("/run/live"));
            let fs = str_of(p, "fstype");
            if fs.is_empty() {
                return;
            }
            let name = [str_of(p, "label"), str_of(p, "partlabel")].into_iter().find(|n| !n.is_empty());
            contents.push(match name {
                Some(n) => format!("{n} ({fs})"),
                None => fs,
            });
        };
        each(d);
        d["children"].as_array().into_iter().flatten().for_each(&mut each);
        out.push(DiskInfo {
            path: str_of(d, "name"),
            model: str_of(d, "model"),
            bytes: d["size"].as_u64().unwrap_or_default(),
            transport: str_of(d, "tran"),
            removable: d["rm"].as_bool().unwrap_or_default(),
            contents,
            live_medium: live,
            windows: None,
        });
    }
    Ok(out)
}

const EFI_GLOBAL: &str = "8be4df61-93ca-11d2-aa0d-00e098032b8c";

/// An EFI variable's value is its 4 attribute bytes followed by the data.
fn efi_flag(dir: &Path, name: &str) -> bool {
    std::fs::read(dir.join(format!("{name}-{EFI_GLOBAL}"))).is_ok_and(|b| b.get(4) == Some(&1))
}

pub fn firmware(sys: &Path) -> Firmware {
    let vars = sys.join("firmware/efi/efivars");
    Firmware {
        efi: sys.join("firmware/efi").is_dir(),
        setup_mode: efi_flag(&vars, "SetupMode"),
        secure_boot_on: efi_flag(&vars, "SecureBoot"),
    }
}

/// Every compiled UTF-8 locale, named in its own language: `Deutsch (Deutschland)`. The ISO ships
/// `locales-all`, which has the compiled archive but no `/usr/share/i18n/SUPPORTED`.
pub fn locales(runner: &dyn Runner) -> Vec<(String, String)> {
    let names = r#"locale -a | while read -r loc; do
	case "$loc" in C.* | POSIX) continue ;; *.utf8 | *.utf8@*) ;; *) continue ;; esac
	n=$(LC_ALL=$loc locale lang_name country_name 2>/dev/null | paste -sd'|')
	printf '%s|%s\n' "$loc" "$n"
done"#;
    let out = runner.run(&Cmd::new("sh").args(["-c", names])).map(|o| o.stdout).unwrap_or_default();
    let mut all: Vec<_> = out.lines().filter_map(native_name).collect();
    all.sort_by_key(|a| a.1.to_lowercase());
    all
}

/// `de_DE.UTF-8|Deutsch|Deutschland` as `("de_DE.UTF-8", "Deutsch (Deutschland)")`.
fn native_name(line: &str) -> Option<(String, String)> {
    let mut f = line.split('|');
    let loc = f.next()?.replacen(".utf8", ".UTF-8", 1);
    let Some(lang) = f.next().filter(|s| !s.is_empty()) else { return Some((loc.clone(), loc)) };
    let country = f.next().filter(|s| !s.is_empty());
    let mut chars = lang.chars();
    let first = chars.next()?.to_uppercase().collect::<String>() + chars.as_str();
    Some(match country {
        Some(c) => (loc, format!("{first} ({c})")),
        None => (loc, first),
    })
}

/// XKB layouts with their descriptions, `("us", "English (US)")`, from the `! layout` section of
/// the XKB rules, sorted by description.
pub fn keymaps(rules: &str) -> Vec<(String, String)> {
    let mut maps: Vec<(String, String)> = rules
        .lines()
        .skip_while(|l| l.trim() != "! layout")
        .skip(1)
        .take_while(|l| !l.starts_with('!'))
        .filter_map(|l| {
            let (name, desc) = l.trim().split_once(char::is_whitespace)?;
            Some((name.to_string(), desc.trim().to_string()))
        })
        .collect();
    maps.sort_by(|a, b| a.1.cmp(&b.1));
    maps
}

/// Zone names from zone1970.tab, plus UTC.
pub fn timezones(tab: &str) -> Vec<String> {
    let mut zones: Vec<String> = tab
        .lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.split('\t').nth(2).map(str::to_string))
        .chain(["UTC".to_string()])
        .collect();
    zones.sort();
    zones
}

/// The git command line for cloning with `auth`. A token goes in as an HTTP header for this one
/// command, so it never lands in the clone's config.
pub fn clone_cmd(url: &str, auth: &GitAuth, into: &Path) -> Cmd {
    let mut git = Cmd::new("git").env("GIT_TERMINAL_PROMPT", "0");
    match auth {
        GitAuth::None => {}
        GitAuth::SshKey(key) => {
            git = git.env(
                "GIT_SSH_COMMAND",
                format!("ssh -i {} -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new", key.display()),
            )
        }
        // GitHub takes any user name with a token and GitLab wants oauth2, so oauth2 serves both.
        GitAuth::Token(t) => {
            let basic = STANDARD.encode(format!("oauth2:{}", t.0));
            git = git.arg("-c").arg(format!("http.extraHeader=Authorization: Basic {basic}"))
        }
    }
    git.arg("clone").arg(url).arg(into.display().to_string())
}

pub fn clone_spec(runner: &dyn Runner, url: &str, auth: &GitAuth, into: &Path) -> Result<Vec<HostSummary>> {
    runner.check(&clone_cmd(url, auth, into))?;
    let spec = fspec_eval::evaluate(&into.join("spec.star"))?;
    Ok(spec.hosts.iter().map(summary).collect())
}

fn summary(h: &fspec_core::resource::Host) -> HostSummary {
    let distro = match &h.system {
        Some(System::Image(i)) => i.distro(),
        Some(System::Distro(d)) => d.distro.clone(),
        None => String::new(),
    };
    let unsupported = if h.platform != "x86_64-linux" {
        Some(format!("This ISO installs x86_64 Linux hosts; this one is {}.", h.platform))
    } else if h.system.is_none() {
        Some("It declares no system, so there is nothing to install.".to_string())
    } else {
        None
    };
    HostSummary {
        name: h.name.clone(),
        platform: h.platform.clone(),
        distro,
        users: h.users.iter().map(|u| u.name.clone()).collect(),
        unsupported,
    }
}

/// The backend on the installer ISO.
pub struct Live {
    paths: Paths,
}

impl Live {
    pub fn new() -> Live {
        Live { paths: Paths::new("/".into(), "/root".into()) }
    }

    fn prepare(&self, c: &Choices) -> Result<(PathBuf, String)> {
        Ok(match &c.system {
            SystemChoice::Fresh { .. } => {
                let dir = PathBuf::from(WORK).join("spec");
                if dir.exists() {
                    std::fs::remove_dir_all(&dir)?;
                }
                std::fs::create_dir_all(&dir)?;
                spec::write_repo(&dir, c)?;
                (dir, c.account.hostname.clone())
            }
            SystemChoice::Restore { checkout, host, .. } => (checkout.clone(), host.clone()),
        })
    }

    fn plan(&self, c: &Choices, ids: &fspec_switch::Ids) -> Result<(fspec_switch::Plan, String)> {
        let (spec_dir, host) = self.prepare(c)?;
        let distro = fspec_eval::evaluate(&spec_dir.join("spec.star"))?.host(&host)?.system.as_ref().map(|s| match s {
            System::Image(i) => i.distro(),
            System::Distro(d) => d.distro.clone(),
        });
        let fedora = match distro.as_deref() {
            Some("fedora") => {
                let json = LiveRunner.check(&Cmd::new("curl").args(["-fsSL", FEDORA_RELEASES]))?.stdout;
                Some(install::fedora_release(&json).context("reading Fedora's release list")?)
            }
            _ => None,
        };
        let exe = std::env::current_exe()?;
        let fspec = exe.with_file_name("fspec");
        let firmware = firmware(Path::new("/sys"));
        let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
        let setup = install::Setup {
            spec_dir: &spec_dir,
            host: &host,
            fspec: &fspec,
            efi: firmware.efi,
            setup_mode: firmware.setup_mode,
            cmdline: &cmdline,
            fedora_release: fedora.as_deref(),
            ids,
        };
        Ok((install::plan(&self.paths, c, &setup)?, distro.unwrap_or_default()))
    }
}

impl Default for Live {
    fn default() -> Live {
        Live::new()
    }
}

/// Where the live session keeps the spec repo it writes and clones.
const WORK: &str = "/run/fspec-install";
const FEDORA_RELEASES: &str = "https://fedoraproject.org/releases.json";

impl Backend for Live {
    fn disks(&self) -> Result<Vec<DiskInfo>> {
        disks(&LiveRunner)
    }

    fn firmware(&self) -> Firmware {
        firmware(Path::new("/sys"))
    }

    fn locales(&self) -> Vec<(String, String)> {
        locales(&LiveRunner)
    }

    fn keymaps(&self) -> Vec<(String, String)> {
        keymaps(&std::fs::read_to_string("/usr/share/X11/xkb/rules/base.lst").unwrap_or_default())
    }

    fn timezones(&self) -> Vec<String> {
        timezones(&std::fs::read_to_string("/usr/share/zoneinfo/zone1970.tab").unwrap_or_default())
    }

    /// The session runs in sway, which switches its layout at runtime; on a bare console the
    /// console keymap is what typing goes through.
    fn set_keymap(&self, keymap: &str) -> Result<()> {
        let cmd = if std::env::var_os("SWAYSOCK").is_some() {
            Cmd::new("swaymsg").args(["input", "type:keyboard", "xkb_layout", keymap])
        } else {
            Cmd::new("loadkeys").arg(keymap)
        };
        LiveRunner.check(&cmd).map(drop)
    }

    fn clone_spec(&self, url: &str, auth: &GitAuth, into: &Path) -> Result<Vec<HostSummary>> {
        if into.exists() {
            std::fs::remove_dir_all(into)?;
        }
        clone_spec(&LiveRunner, url, auth, into)
    }

    fn fetch_key(&self, source: &keys::Source) -> Result<keys::Key> {
        keys::fetch(&LiveRunner, source, &std::env::current_exe()?)
    }

    fn preview(&self, draft: &Draft) -> String {
        spec::preview(draft)
    }

    /// The ISO's boot check waits for this line on the serial console. The serial entries boot with
    /// loglevel=3, since a chattier console stalls the VM, so it is logged as critical to get through.
    fn shown(&self) {
        let _ = std::fs::write("/dev/kmsg", "<2>fspec-install: first screen\n");
    }

    fn install(&self, c: &Choices, tx: Sender<Progress>) {
        let log = install::log_path();
        let planned = fspec_switch::new_ids().and_then(|ids| self.plan(c, &ids));
        let (plan, distro) = match planned {
            Ok(p) => p,
            Err(e) => {
                let stage = "Preparing the install".to_string();
                let _ = tx.send(Progress::Failed { stage, error: format!("{e:#}"), log });
                return;
            }
        };
        if install::execute(&plan, &c.disk.device, &distro, &log, &tx).is_ok() {
            let _ = tx.send(Progress::Done);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_disks_with_their_contents_and_marks_the_live_medium() {
        let json = r#"{"blockdevices":[
          {"name":"/dev/nvme0n1","type":"disk","size":1000204886016,"model":"Samsung SSD 980 PRO 1TB ","tran":"nvme","rm":false,"fstype":null,"label":null,"partlabel":null,"mountpoints":[null],
           "children":[
             {"name":"/dev/nvme0n1p1","type":"part","size":104857600,"model":null,"tran":null,"rm":false,"fstype":"vfat","label":null,"partlabel":"EFI system partition","mountpoints":[null]},
             {"name":"/dev/nvme0n1p2","type":"part","size":16777216,"model":null,"tran":null,"rm":false,"fstype":null,"label":null,"partlabel":"Microsoft reserved partition","mountpoints":[null]},
             {"name":"/dev/nvme0n1p3","type":"part","size":999000000000,"model":null,"tran":null,"rm":false,"fstype":"ntfs","label":"Windows","partlabel":"Basic data partition","mountpoints":[null]}]},
          {"name":"/dev/sda","type":"disk","size":16008609792,"model":"Cruzer","tran":"usb","rm":true,"fstype":"iso9660","label":"FORMALSPEC","partlabel":null,"mountpoints":["/run/live/medium"]},
          {"name":"/dev/sr0","type":"rom","size":1073741312,"model":"QEMU DVD","tran":"sata","rm":true,"fstype":null,"label":null,"partlabel":null,"mountpoints":[null]},
          {"name":"/dev/vdb","type":"disk","size":21474836480,"model":null,"tran":null,"rm":false,"fstype":null,"label":null,"partlabel":null,"mountpoints":[null]}]}"#;
        let d = parse_disks(json).unwrap();
        assert_eq!(d.len(), 3);
        assert_eq!(d[0].model, "Samsung SSD 980 PRO 1TB");
        assert_eq!(d[0].contents, ["EFI system partition (vfat)", "Windows (ntfs)"]);
        assert!(!d[0].live_medium);
        assert!(d[1].live_medium && d[1].removable);
        assert_eq!((d[2].path.as_str(), d[2].contents.len(), d[2].bytes), ("/dev/vdb", 0, 21474836480));
    }

    #[test]
    fn reads_setup_mode_and_secure_boot_from_efivars() {
        let sys = tempfile::tempdir().unwrap();
        let vars = sys.path().join("firmware/efi/efivars");
        std::fs::create_dir_all(&vars).unwrap();
        std::fs::write(vars.join(format!("SetupMode-{EFI_GLOBAL}")), [7, 0, 0, 0, 1]).unwrap();
        std::fs::write(vars.join(format!("SecureBoot-{EFI_GLOBAL}")), [7, 0, 0, 0, 0]).unwrap();
        assert_eq!(firmware(sys.path()), Firmware { efi: true, setup_mode: true, secure_boot_on: false });
        assert!(!firmware(&sys.path().join("none")).efi);
    }

    #[test]
    fn names_locales_in_their_own_language() {
        assert_eq!(native_name("de_DE.UTF-8|Deutsch|Deutschland").unwrap().1, "Deutsch (Deutschland)");
        assert_eq!(native_name("eo.UTF-8|esperanto|").unwrap().1, "Esperanto");
        assert_eq!(native_name("xx.UTF-8||").unwrap().1, "xx.UTF-8");
        assert_eq!(
            native_name("es_ES.utf8|español|España").unwrap(),
            ("es_ES.UTF-8".into(), "Español (España)".into())
        );
        assert_eq!(native_name("ca_ES.utf8@valencia|català|Espanya").unwrap().0, "ca_ES.UTF-8@valencia");
    }

    #[test]
    fn reads_layouts_and_zones() {
        let rules = "! model\n  pc105 Generic\n\n! layout\n  us              English (US)\n  de              German\n\n! variant\n  intl us: x\n";
        assert_eq!(keymaps(rules), [("us".into(), "English (US)".into()), ("de".into(), "German".into())]);
        let tab = "# comment\nES\t+4024-00341\tEurope/Madrid\nNL,BE\t+5222+00454\tEurope/Brussels\tBelgium\n";
        assert_eq!(timezones(tab), ["Europe/Brussels", "Europe/Madrid", "UTC"]);
    }

    #[test]
    fn a_token_rides_in_a_header_and_a_key_in_the_ssh_command() {
        let into = Path::new("/root/spec");
        let tok = clone_cmd("https://github.com/a/b", &GitAuth::Token(crate::model::Secret("t0k".into())), into);
        assert!(tok.args[1].starts_with("http.extraHeader=Authorization: Basic "));
        assert!(!tok.args.iter().any(|a| a.contains("t0k")));
        let key = clone_cmd("git@github.com:a/b", &GitAuth::SshKey("/root/.ssh/id".into()), into);
        assert!(key.env.iter().any(|(k, v)| k == "GIT_SSH_COMMAND" && v.contains("-i /root/.ssh/id")));
        assert_eq!(key.args, ["clone", "git@github.com:a/b", "/root/spec"]);
    }
}
