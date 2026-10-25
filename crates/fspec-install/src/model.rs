//! What the installer collects before it touches a disk.

use std::fmt;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Release {
    Rolling,
    Fixed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Init {
    Systemd,
    Runit,
}

/// How the package manager changes the system: in place, or atomically.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    Classic,
    /// The package manager swaps the whole `/usr` in one transaction and keeps earlier states.
    Atomic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Preset {
    Minimal,
    Desktop,
    Gaming,
    Server,
}

impl Preset {
    /// The module file and function name in the written spec repo.
    pub fn id(self) -> &'static str {
        match self {
            Preset::Minimal => "minimal",
            Preset::Desktop => "desktop",
            Preset::Gaming => "gaming",
            Preset::Server => "server",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Desktop {
    Gnome,
    Kde,
    Cosmic,
}

impl Desktop {
    pub fn id(self) -> &'static str {
        match self {
            Desktop::Gnome => "gnome",
            Desktop::Kde => "kde",
            Desktop::Cosmic => "cosmic",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Desktop::Gnome => "GNOME",
            Desktop::Kde => "KDE Plasma",
            Desktop::Cosmic => "COSMIC",
        }
    }
}

/// A distro the installer offers. `id` is the name the written spec and `disk init --distro` use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Distro {
    pub id: &'static str,
    pub name: &'static str,
    /// One line on why someone picks it.
    pub reason: &'static str,
    pub release: Release,
    pub init: Init,
    /// The kernel carries a vendor signature that a stock Secure Boot setup trusts.
    pub signed_kernel: bool,
    pub delivery: Delivery,
    /// The distro it builds on, or `None` for an independent one.
    pub based_on: Option<&'static str>,
    /// Not released as stable yet, in its developers' own word: `alpha`, `release candidate`.
    pub prerelease: Option<&'static str>,
    /// Ships snapper, taking btrfs snapshots around every change that the boot menu offers.
    pub snapper: bool,
    /// Installs only on the UEFI layouts, never the BIOS one.
    pub needs_efi: bool,
    pub presets: &'static [Preset],
    /// The desktops its desktop and gaming presets come in. The first is the default.
    pub desktops: &'static [Desktop],
}

/// A value that never shows up in logs or `Debug` output.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Secret(pub String);

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(..)")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Locale {
    /// `en_US.UTF-8`.
    pub locale: String,
    /// Console keymap, `us`, `de-latin1`.
    pub keymap: String,
    /// `Europe/Madrid`.
    pub timezone: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskChoice {
    /// The whole disk, `/dev/nvme0n1`.
    pub device: String,
    /// Put the btrfs filesystem in LUKS2 with this passphrase.
    pub encrypt: Option<Secret>,
    pub swap_gib: u64,
    /// Install into space beside Windows instead of erasing the disk.
    pub windows: Option<fspec_switch::BesideWindows>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitAuth {
    None,
    /// A private key file the user pointed at, or pasted into one under the live session's home.
    SshKey(PathBuf),
    /// An HTTPS access token.
    Token(Secret),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SystemChoice {
    /// A new spec written from a distro and a preset.
    Fresh { distro: &'static str, preset: Preset, desktop: Option<Desktop> },
    /// An existing spec repo, cloned to `checkout`, installed as host `host` declares it.
    Restore { url: String, auth: GitAuth, checkout: PathBuf, host: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    /// Login name.
    pub name: String,
    pub full_name: String,
    pub hostname: String,
    /// Set once with chpasswd in the new root; never written to the spec.
    pub password: Secret,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choices {
    pub locale: Locale,
    pub disk: DiskChoice,
    pub system: SystemChoice,
    pub account: Account,
    /// Enrol our own keys with sbctl and sign the boot manager and the kernels with them.
    pub secure_boot: bool,
}

/// The choices made so far. The front end fills it screen by screen and renders the spec it would
/// write from whatever is set.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Draft {
    pub locale: Option<Locale>,
    pub disk: Option<DiskChoice>,
    pub system: Option<SystemChoice>,
    pub account: Option<Account>,
    pub secure_boot: bool,
}

impl Draft {
    pub fn complete(&self) -> Option<Choices> {
        Some(Choices {
            locale: self.locale.clone()?,
            disk: self.disk.clone()?,
            system: self.system.clone()?,
            account: self.account.clone()?,
            secure_boot: self.secure_boot,
        })
    }
}

/// A host found in a cloned spec, for the restore picker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostSummary {
    pub name: String,
    pub platform: String,
    /// `fedora`, `bazzite`, …; empty when the host declares no system.
    pub distro: String,
    /// Login names it declares.
    pub users: Vec<String>,
    /// Why it cannot be installed from here, e.g. a macOS host.
    pub unsupported: Option<String>,
}

/// Firmware facts the review screen needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Firmware {
    pub efi: bool,
    /// Secure Boot is in setup mode, so sbctl can enrol keys.
    pub setup_mode: bool,
    pub secure_boot_on: bool,
}

/// A whole disk the installer can install onto, as lsblk describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskInfo {
    pub path: String,
    pub model: String,
    pub bytes: u64,
    /// `nvme`, `sata`, `usb`, `virtio`.
    pub transport: String,
    pub removable: bool,
    /// Labels and filesystems of what is on it now, `EFI (vfat)`, `root (btrfs)`. Empty when blank.
    pub contents: Vec<String>,
    /// The live session booted from it, so it cannot be the target.
    pub live_medium: bool,
    /// It holds Windows, which the install can keep.
    pub windows: Option<fspec_switch::WindowsDisk>,
}

/// What the install reports while it runs, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Progress {
    /// A new stage started. `index` counts from 0 up to `total`.
    Stage {
        index: usize,
        total: usize,
        title: String,
    },
    /// A line a command printed.
    Output(String),
    Done,
    /// The install stopped. `log` holds every command and its output.
    Failed {
        stage: String,
        error: String,
        log: PathBuf,
    },
}
