//! A backend that touches nothing, so the front end runs on any computer with `--demo`.

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::thread::sleep;
use std::time::Duration;

use anyhow::{Result, bail};

use crate::backend::Backend;
use crate::model::{Choices, DiskInfo, Draft, Firmware, GitAuth, HostSummary, Progress, SystemChoice};

const STAGES: [&str; 12] = [
    "Partition the disk",
    "Format the partitions",
    "Set up LUKS2",
    "Create btrfs subvolumes",
    "Fetch the base system",
    "Bootstrap the base system",
    "Configure locale and time zone",
    "Install the FormalSpec tools",
    "Apply your spec",
    "Set your password",
    "Build the kernels",
    "Install the bootloader",
];

pub struct DemoBackend {
    clone_delay: Duration,
    stage_delay: Duration,
    fail_at: Option<usize>,
    efi: bool,
    bitlocker: bool,
}

impl DemoBackend {
    /// Realistic timing: a 1.5 s clone and an install of about 20 s. Fails at stage 6 when
    /// `FSPEC_INSTALL_DEMO_FAIL=1`.
    pub fn new() -> Self {
        let fail = std::env::var("FSPEC_INSTALL_DEMO_FAIL").is_ok_and(|v| v == "1");
        DemoBackend {
            clone_delay: Duration::from_millis(1500),
            stage_delay: Duration::from_millis(1700),
            fail_at: fail.then_some(5),
            efi: true,
            bitlocker: false,
        }
    }

    /// No delays, for tests.
    pub fn instant() -> Self {
        DemoBackend {
            clone_delay: Duration::ZERO,
            stage_delay: Duration::ZERO,
            fail_at: None,
            efi: true,
            bitlocker: false,
        }
    }

    pub fn with_stage_delay(mut self, d: Duration) -> Self {
        self.stage_delay = d;
        self
    }

    /// A machine that booted with BIOS.
    pub fn bios(mut self) -> Self {
        self.efi = false;
        self
    }

    /// Windows on the first disk is BitLocker-encrypted, so it cannot be shrunk.
    pub fn bitlocker(mut self) -> Self {
        self.bitlocker = true;
        self
    }

    pub fn failing_at(mut self, stage: usize) -> Self {
        self.fail_at = Some(stage);
        self
    }
}

impl Default for DemoBackend {
    fn default() -> Self {
        Self::new()
    }
}

fn disk(path: &str, model: &str, bytes: u64, transport: &str, contents: &[&str], live: bool) -> DiskInfo {
    DiskInfo {
        path: path.into(),
        model: model.into(),
        bytes,
        transport: transport.into(),
        removable: live,
        contents: contents.iter().map(|s| s.to_string()).collect(),
        live_medium: live,
        windows: None,
    }
}

/// Windows filling a 1 TB disk: a 100 MiB ESP, `C:` and its recovery partition at the end.
fn windows(bitlocker: bool) -> fspec_switch::WindowsDisk {
    let gib = fspec_switch::GIB;
    let part = |dev: &str, partn, start, size, fstype: &str| fspec_switch::Part {
        dev: dev.into(),
        partn,
        start,
        size,
        fstype: fstype.into(),
        ..fspec_switch::Part::default()
    };
    let c_size = 1_000_204_886_016 - 2 * gib;
    fspec_switch::WindowsDisk {
        disk: "/dev/nvme0n1".into(),
        sector: 512,
        esp: part("/dev/nvme0n1p1", 1, 2048, 100 << 20, "vfat"),
        windows: part("/dev/nvme0n1p3", 3, 239616, c_size, if bitlocker { "BitLocker" } else { "ntfs" }),
        windows_limit: 239616 + c_size / 512,
        gaps: Vec::new(),
        next_partn: 5,
        shrink_min: Ok(300_000_000_000),
    }
}

fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
    items.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

/// What the demo pretends `clone_spec` fetched.
const CLONED: &str = r#"load("//modules/cli.star", "cli")
load("//modules/desktop.star", "desktop")

def machine(name, platform, system, extra = []):
    return host(
        name = name,
        platform = platform,
        system = system,
        users = [user("kyan", uid = 1000, shell = "fish")],
        modules = [cli] + extra,
    )

hosts = [
    machine("framework", "x86_64-linux", fedora(packages = ["NetworkManager"]), [desktop]),
    machine("macbook", "aarch64-darwin", None),
]
"#;

impl Backend for DemoBackend {
    fn disks(&self) -> Result<Vec<DiskInfo>> {
        let mut win = disk(
            "/dev/nvme0n1",
            "Samsung SSD 980 PRO",
            1_000_204_886_016,
            "nvme",
            &[if self.bitlocker { "Windows (BitLocker)" } else { "Windows (ntfs)" }, "EFI (vfat)"],
            false,
        );
        win.windows = Some(windows(self.bitlocker));
        Ok(vec![
            win,
            disk("/dev/sda", "WD Blue SA510", 500_107_862_016, "sata", &[], false),
            disk("/dev/sdb", "SanDisk Ultra", 16_008_609_792, "usb", &["FSPEC-LIVE (iso9660)"], true),
        ])
    }

    fn firmware(&self) -> Firmware {
        Firmware { efi: self.efi, setup_mode: self.efi, secure_boot_on: false }
    }

    fn locales(&self) -> Vec<(String, String)> {
        pairs(&[
            ("en_US.UTF-8", "English (United States)"),
            ("en_GB.UTF-8", "English (United Kingdom)"),
            ("de_DE.UTF-8", "Deutsch (Deutschland)"),
            ("fr_FR.UTF-8", "Français (France)"),
            ("es_ES.UTF-8", "Español (España)"),
            ("it_IT.UTF-8", "Italiano (Italia)"),
            ("nl_NL.UTF-8", "Nederlands (Nederland)"),
            ("pt_BR.UTF-8", "Português (Brasil)"),
            ("pl_PL.UTF-8", "Polski (Polska)"),
            ("sv_SE.UTF-8", "Svenska (Sverige)"),
            ("ja_JP.UTF-8", "日本語 (日本)"),
            ("ru_RU.UTF-8", "Русский (Россия)"),
        ])
    }

    fn keymaps(&self) -> Vec<String> {
        strings(&[
            "us",
            "uk",
            "de",
            "de-latin1",
            "fr",
            "fr-latin9",
            "es",
            "it",
            "nl",
            "pt-latin1",
            "br-abnt2",
            "pl2",
            "se-lat6",
            "no",
            "dk",
            "fi",
            "ru",
            "cz",
            "us-dvorak",
            "us-colemak",
        ])
    }

    fn timezones(&self) -> Vec<String> {
        strings(&[
            "UTC",
            "Europe/Amsterdam",
            "Europe/Berlin",
            "Europe/Brussels",
            "Europe/Copenhagen",
            "Europe/Dublin",
            "Europe/Helsinki",
            "Europe/Lisbon",
            "Europe/London",
            "Europe/Madrid",
            "Europe/Paris",
            "Europe/Rome",
            "Europe/Stockholm",
            "Europe/Warsaw",
            "Europe/Moscow",
            "America/New_York",
            "America/Chicago",
            "America/Denver",
            "America/Los_Angeles",
            "America/Sao_Paulo",
            "America/Toronto",
            "Asia/Tokyo",
            "Asia/Seoul",
            "Asia/Shanghai",
            "Asia/Singapore",
            "Asia/Kolkata",
            "Asia/Dubai",
            "Australia/Sydney",
            "Pacific/Auckland",
            "Africa/Johannesburg",
        ])
    }

    fn set_keymap(&self, _keymap: &str) -> Result<()> {
        Ok(())
    }

    fn clone_spec(&self, url: &str, _auth: &GitAuth, _into: &Path) -> Result<Vec<HostSummary>> {
        sleep(self.clone_delay);
        if url.contains("fail") {
            bail!("Cloning into 'spec'...\nfatal: repository '{url}' not found");
        }
        Ok(vec![
            HostSummary {
                name: "framework".into(),
                platform: "x86_64-linux".into(),
                distro: "fedora".into(),
                users: vec!["kyan".into()],
                unsupported: None,
            },
            HostSummary {
                name: "macbook".into(),
                platform: "aarch64-darwin".into(),
                distro: String::new(),
                users: vec!["kyan".into()],
                unsupported: Some("macOS hosts are not installed from this ISO".into()),
            },
        ])
    }

    fn preview(&self, draft: &Draft) -> String {
        match draft.system {
            Some(SystemChoice::Restore { .. }) => CLONED.to_string(),
            _ => crate::spec::preview(draft),
        }
    }

    fn install(&self, _choices: &Choices, tx: Sender<Progress>) {
        let total = STAGES.len();
        for (index, title) in STAGES.iter().enumerate() {
            let _ = tx.send(Progress::Stage { index, total, title: title.to_string() });
            for n in 1..=3 {
                sleep(self.stage_delay / 4);
                let _ =
                    tx.send(Progress::Output(format!("[{}/{}] {} ({n}/3)", index + 1, total, title.to_lowercase())));
            }
            if self.fail_at == Some(index) {
                sleep(self.stage_delay / 4);
                let log = PathBuf::from("/var/log/fspec-install.log");
                let _ = tx.send(Progress::Failed {
                    stage: title.to_string(),
                    error: "dnf exited with status 1: Failed to download metadata for repo \"fedora\".".into(),
                    log,
                });
                return;
            }
            sleep(self.stage_delay / 4);
        }
        let _ = tx.send(Progress::Done);
    }
}
