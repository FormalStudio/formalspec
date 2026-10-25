use anyhow::{Result, bail};
use fspec_core::runner::Cmd;

use crate::DISTROS;
use crate::classic::Target;
use crate::machine::{Firmware, Layout};

/// What a distro needs from the disk to be switched to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recipe {
    pub distro: &'static str,
    /// The smallest ESP it fits on next to the other roots.
    pub min_esp_mib: u64,
    /// Room its kernels take on the ESP or in the shared boot partition.
    pub kernel_mib: u64,
    /// Builds unified kernel images on the default layout.
    pub uki: bool,
    /// Boots from ZFS with the distro's own packages.
    pub zfs: bool,
    /// Boots only through systemd-boot, so neither ZFSBootMenu nor BIOS GRUB.
    pub only_esp: bool,
}

const RECIPES: [Recipe; 6] = [
    // Fedora keeps three kernels, each a UKI with a full dracut initramfs.
    Recipe { distro: "fedora", min_esp_mib: 1024, kernel_mib: 400, uki: true, zfs: false, only_esp: false },
    Recipe { distro: "arch", min_esp_mib: 512, kernel_mib: 150, uki: true, zfs: false, only_esp: false },
    Recipe { distro: "debian", min_esp_mib: 512, kernel_mib: 200, uki: true, zfs: true, only_esp: false },
    Recipe { distro: "ubuntu", min_esp_mib: 512, kernel_mib: 250, uki: true, zfs: true, only_esp: false },
    Recipe { distro: "void", min_esp_mib: 256, kernel_mib: 100, uki: false, zfs: true, only_esp: false },
    // Each kernel ships a 200 MB generic initramfs, and blsforme keeps the kernels of the last five
    // moss states on the ESP. It boots only through systemd-boot.
    Recipe { distro: "aerynos", min_esp_mib: 1024, kernel_mib: 450, uki: false, zfs: false, only_esp: true },
];

/// Where AerynOS's moss bootstraps from: the stream its own installer uses. moss rewrites this
/// index URL to the root-index form itself.
const AERYNOS_REPO: &str = "https://cdn.aerynos.dev/stream/unstable/x86_64/stone.index";

/// AerynOS's own metapackage for a bootable base, a kernel with firmware, and what the layout
/// needs. `systemd` carries systemd-boot, and the kernel package a prebuilt initramfs.
const AERYNOS_BASE: [&str; 4] = ["pkgset-aeryn-base", "linux-stable", "linux-firmware", "btrfs-progs"];

/// The file the distro's kernel tooling reads the kernel options from. blsforme appends
/// `/etc/kernel/cmdline.d` to the root it detects, and writes `rootfsflags=`, which neither dracut
/// nor systemd reads, so the root's subvolume has to come from here.
pub(crate) fn cmdline_file(distro: &str) -> &'static str {
    match distro {
        "aerynos" => "/etc/kernel/cmdline.d/fspec.cmdline",
        _ => "/etc/kernel/cmdline",
    }
}

/// moss with the shared ESP's default entry kept; see [`fspec_core::moss`].
pub(crate) fn moss<I: IntoIterator<Item = S>, S: Into<String>>(esp: &str, args: I) -> Cmd {
    fspec_core::moss::moss(&format!("{esp}/loader/loader.conf"), args)
}

pub fn recipe(distro: &str) -> Result<&'static Recipe> {
    match RECIPES.iter().find(|r| r.distro == distro) {
        Some(r) => Ok(r),
        None => bail!("cannot bootstrap `{distro}`; --distro takes one of {}", DISTROS.join(", ")),
    }
}

/// Which bootloader setup a root gets, from the layout it lands on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Boot {
    /// systemd-boot on the ESP at `/efi`.
    Esp,
    /// ZFSBootMenu, reading the kernels from the root's own `/boot`.
    Zbm,
    /// BIOS GRUB with the root's `/boot` in the shared boot partition.
    Grub,
}

impl Boot {
    pub fn of(layout: &Layout) -> Option<Boot> {
        match layout {
            Layout::Default { .. } => Some(Boot::Esp),
            Layout::Zfs { .. } => Some(Boot::Zbm),
            Layout::Bios { .. } => Some(Boot::Grub),
            Layout::Unsupported { .. } => None,
        }
    }
}

/// Packages the bootstrap adds for the boot setup. Debian and Ubuntu install theirs with apt in
/// [`populate`] instead, since DKMS and debconf behave there and not inside debootstrap.
pub(crate) fn boot_packages(distro: &str, boot: Boot, luks: bool) -> Vec<&'static str> {
    let mut pkgs = match (distro, boot) {
        ("arch", Boot::Esp) => vec!["systemd-ukify"],
        ("arch", _) => vec!["grub"],
        ("fedora", Boot::Esp) => vec!["systemd-ukify", "systemd-boot-unsigned"],
        ("fedora", _) => vec!["grub2-tools", "grub2-pc"],
        ("void", Boot::Grub) => vec!["grub"],
        ("void", Boot::Zbm) => vec!["zfs", "linux-headers"],
        _ => Vec::new(),
    };
    if luks && !matches!(distro, "debian" | "ubuntu") {
        pkgs.push("cryptsetup");
    }
    pkgs
}

/// apt packages for Debian and Ubuntu, installed in the new root by [`populate`].
fn apt_packages(distro: &str, arch: &str, boot: Boot, luks: bool) -> Vec<String> {
    let deb = if arch == "aarch64" { "arm64" } else { "amd64" };
    let mut pkgs: Vec<String> = match (distro, boot) {
        (_, Boot::Esp) => vec!["systemd-ukify".into(), "systemd-boot-efi".into()],
        (_, Boot::Grub) => vec!["grub-pc".into()],
        ("ubuntu", Boot::Zbm) => vec!["zfsutils-linux".into(), "zfs-initramfs".into()],
        (_, Boot::Zbm) => vec!["zfs-dkms".into(), "zfs-initramfs".into(), format!("linux-headers-{deb}")],
    };
    if luks {
        pkgs.extend(["cryptsetup".into(), "cryptsetup-initramfs".into()]);
    }
    pkgs
}

/// The command that installs the base system into `root`.
/// The ESP the new root boots from, at `esp`, is mounted at `<root>/efi` before this runs, since
/// moss syncs boot as part of the install.
pub fn bootstrap(t: &Target, boot: Boot, luks: bool, root: &str, esp: &str) -> Result<Vec<Cmd>> {
    let arch = t.arch;
    let extra = boot_packages(t.distro, boot, luks);
    Ok(match t.distro {
        "fedora" => {
            let Some(rel) = t.release else { bail!("fedora needs --release <number>, the Fedora release to install") };
            // dnf reads repositories from the installroot, which has none yet, so they are named here.
            // Arch's dnf is dnf 4 and Fedora's is dnf5; both take these options. Weak dependencies
            // stay on as in a Fedora install: systemd-pam (pam_systemd, so logind sessions and the
            // user bus exist) and systemd-resolved are only Recommends of systemd.
            let mirror = "https://download.fedoraproject.org/pub/fedora/linux";
            let key = format!(
                "https://src.fedoraproject.org/rpms/fedora-repos/raw/rawhide/f/RPM-GPG-KEY-fedora-{rel}-primary"
            );
            let mut dnf = Cmd::new("dnf").args([
                "--installroot",
                root,
                "--releasever",
                rel,
                "-y",
                "--setopt=max_parallel_downloads=10",
            ]);
            for (id, path) in [
                ("fedora", format!("releases/{rel}/Everything/{arch}/os")),
                ("updates", format!("updates/{rel}/Everything/{arch}")),
            ] {
                dnf = dnf
                    .arg(format!("--repofrompath={id},{mirror}/{path}"))
                    .arg(format!("--setopt={id}.gpgcheck=1"))
                    .arg(format!("--setopt={id}.gpgkey={key}"));
            }
            let base =
                ["systemd", "dbus-broker", "dnf", "fedora-release", "fedora-repos", "passwd", "btrfs-progs", "kernel"];
            vec![dnf.arg("install").args(base).args(extra.iter().copied())]
        }
        "arch" => {
            vec![
                Cmd::new("pacstrap")
                    .args(["-K", root, "base", "linux", "linux-firmware", "btrfs-progs"])
                    .args(extra.iter().copied()),
            ]
        }
        "debian" | "ubuntu" => {
            let deb = if arch == "aarch64" { "arm64" } else { "amd64" };
            let (kernel, suite, components, mirror) = match t.distro {
                "debian" => (format!("linux-image-{deb}"), "stable", "main,contrib", "http://deb.debian.org/debian"),
                _ if arch == "aarch64" => {
                    ("linux-image-generic".into(), "noble", "main,universe", "http://ports.ubuntu.com/ubuntu-ports")
                }
                _ => ("linux-image-generic".into(), "noble", "main,universe", "http://archive.ubuntu.com/ubuntu"),
            };
            vec![
                Cmd::new("debootstrap")
                    .args(["--arch", deb])
                    .arg(format!("--components={components}"))
                    .arg(format!("--include=systemd-sysv,btrfs-progs,{kernel}"))
                    .args([t.release.unwrap_or(suite), root, mirror]),
            ]
        }
        "void" => vec![
            Cmd::new("xbps-install")
                .args(["-S", "-y", "-r", root, "-R", "https://repo-default.voidlinux.org/current"])
                .args(["base-system", "linux", "btrfs-progs"])
                .args(extra.iter().copied())
                .env("XBPS_ARCH", arch)
                // A fresh root has no repository key yet, and `-y` does not answer the import prompt.
                .stdin("y\n"),
        ],
        "aerynos" => {
            let has_moss = "command -v moss >/dev/null || { echo 'bootstrapping AerynOS needs moss on PATH: the static \
                            build is moss.tar.gz on https://github.com/AerynOS/os-tools/releases' >&2; exit 1; }";
            vec![
                Cmd::new("sh").args(["-c", has_moss]),
                moss(esp, ["-D", root, "-y", "repo", "add", "unstable", AERYNOS_REPO, "-p", "0"]),
                moss(esp, ["-D", root, "-y", "install"].into_iter().chain(AERYNOS_BASE).chain(extra.iter().copied())),
            ]
        }
        other => bail!("cannot bootstrap `{other}`; --distro takes one of {}", DISTROS.join(", ")),
    })
}

const VOID_ESP_HOOK: &str = r#"#!/bin/sh
# Void's kernel hook from fspec: $1 is the kernel package, $2 its version. Void has no UKI tooling,
# so the newest kernel is copied to the ESP under fixed names with its own entry.
set -e
mkdir -p /efi/void /efi/loader/entries
cp "/boot/vmlinuz-$2" /efi/void/vmlinuz
cp "/boot/initramfs-$2.img" /efi/void/initrd
printf 'title Void (fspec)\nlinux /void/vmlinuz\ninitrd /void/initrd\noptions %s\n' "$(cat /etc/kernel/cmdline)" \
	>/efi/loader/entries/void-fspec.conf
"#;

const LINKS_HOOK: &str = r#"#!/bin/sh
# Keeps vmlinuz and initramfs.img pointing at the newest kernel, the names the shared GRUB menu
# boots. Void calls it with the kernel package and version, kernel-install with add and the version.
[ "$1" = remove ] && exit 0
ln -sf "vmlinuz-$2" /boot/vmlinuz
ln -sf "initramfs-$2.img" /boot/initramfs.img
"#;

const DEBIAN_UKI_HOOK: &str = r#"#!/bin/sh
# Builds this kernel's UKI on the ESP after initramfs-tools wrote its initrd. $1 is the version.
exec kernel-install add "$1" "/boot/vmlinuz-$1" "/boot/initrd.img-$1"
"#;

const DEBIAN_UKI_REMOVE: &str = "#!/bin/sh\nexec kernel-install remove \"$1\"\n";

/// A file the boot setup writes into the root, and whether it is executable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct File {
    pub path: &'static str,
    pub text: String,
    pub exec: bool,
}

fn file(path: &'static str, text: impl Into<String>, exec: bool) -> File {
    File { path, text: text.into(), exec }
}

/// The kernel tooling config that makes the distro put its own kernels where the layout boots
/// them, so kernel updates keep working without fspec.
pub fn boot_files(distro: &str, boot: Boot, luks: bool) -> Vec<File> {
    let mut files = Vec::new();
    match (distro, boot) {
        ("arch", Boot::Esp) => files.push(file(
            "/etc/mkinitcpio.d/linux.preset",
            "ALL_kver=\"/boot/vmlinuz-linux\"\nPRESETS=('default')\ndefault_uki=\"/efi/EFI/Linux/arch-linux.efi\"\n",
            false,
        )),
        // BOOT_ROOT skips kernel-install's search, which beside Windows would pick Windows' ESP
        // at /boot/efi instead of the XBOOTLDR partition at /efi.
        ("fedora", Boot::Esp) => {
            files.push(file(
                "/etc/kernel/install.conf",
                "BOOT_ROOT=/efi\nlayout=uki\nuki_generator=ukify\ninitrd_generator=dracut\n",
                false,
            ));
            files.push(file("/etc/kernel/entry-token", "fedora\n", false));
        }
        ("debian" | "ubuntu", Boot::Esp) => {
            files.push(file("/etc/kernel/install.conf", "BOOT_ROOT=/efi\nlayout=uki\nuki_generator=ukify\n", false));
            files.push(file("/etc/kernel/entry-token", format!("{distro}\n"), false));
            files.push(file("/etc/kernel/postinst.d/zz-fspec-uki", DEBIAN_UKI_HOOK, true));
            files.push(file("/etc/initramfs/post-update.d/zz-fspec-uki", DEBIAN_UKI_HOOK, true));
            files.push(file("/etc/kernel/postrm.d/zz-fspec-uki", DEBIAN_UKI_REMOVE, true));
        }
        ("void", Boot::Esp) => files.push(file("/etc/kernel.d/post-install/60-fspec", VOID_ESP_HOOK, true)),
        ("fedora", Boot::Grub) => files.push(file("/etc/kernel/install.d/95-fspec-links.install", LINKS_HOOK, true)),
        ("void", Boot::Grub) => files.push(file("/etc/kernel.d/post-install/60-fspec-links", LINKS_HOOK, true)),
        ("debian" | "ubuntu", Boot::Grub) => {
            files.push(file("/etc/kernel-img.conf", "do_symlinks = yes\nlink_in_boot = yes\n", false))
        }
        _ => {}
    }
    if luks && matches!(distro, "fedora" | "void") {
        files.push(file("/etc/dracut.conf.d/fspec-crypt.conf", "add_dracutmodules+=\" crypt \"\n", false));
    }
    files
}

/// Shell run inside a Debian or Ubuntu root before its first apply: adds the update and security
/// pockets, installs the boot packages and upgrades what debootstrap installed. Run after the
/// apply, the upgrade would move every package the apply just installed past the version the
/// apply recorded, and download it twice.
pub fn upgrade(distro: &str, arch: &str, boot: Boot, luks: bool) -> Option<String> {
    if !matches!(distro, "debian" | "ubuntu") {
        return None;
    }
    let mut sh = vec!["set -e".to_string()];
    // debootstrap leaves only the release pocket, without security fixes.
    let security = if distro == "debian" { "http://security.debian.org/debian-security" } else { "$m" };
    sh.push(format!(
        "read -r _ m s c </etc/apt/sources.list\n\
         printf 'deb %s %s %s\\ndeb %s %s-updates %s\\ndeb %s %s-security %s\\n' \"$m\" \"$s\" \"$c\" \"$m\" \"$s\" \"$c\" \
         \"{security}\" \"$s\" \"$c\" >/etc/apt/sources.list"
    ));
    sh.push("export DEBIAN_FRONTEND=noninteractive".into());
    sh.push("apt-get update".into());
    // ukify and its kernel-install plugin come first: a kernel the upgrade installs without
    // them gets 90-uki-copy.install's "missing .efi suffix" failure.
    sh.push(format!("apt-get install -y --no-install-recommends {}", apt_packages(distro, arch, boot, luks).join(" ")));
    sh.push("apt-get full-upgrade -y".into());
    Some(sh.join("\n") + "\n")
}

/// Shell run inside the root once its packages are in: builds the initramfs and kernel files
/// where the layout boots them.
pub fn populate(distro: &str, boot: Boot, luks: bool) -> String {
    let mut sh = vec!["set -e".to_string()];
    let each = |cmd: &str| format!("for v in /lib/modules/*; do v=${{v##*/}}; {cmd}; done");
    if matches!(distro, "debian" | "ubuntu") {
        // debootstrap installs the kernel before initramfs-tools, so there may be no initramfs
        // to update yet.
        sh.push(each(
            "if [ -e \"/boot/initrd.img-$v\" ]; then update-initramfs -u -k \"$v\"; else update-initramfs -c -k \"$v\"; fi",
        ));
    }
    match (distro, boot) {
        ("arch", Boot::Esp | Boot::Grub) => {
            if luks {
                sh.push("sed -i '/^HOOKS=/s/ block / block sd-encrypt /' /etc/mkinitcpio.conf".into());
            }
            if boot == Boot::Esp {
                sh.push("mkdir -p /efi/EFI/Linux".into());
            }
            sh.push("mkinitcpio -P".into());
        }
        ("fedora", Boot::Esp) => {
            // In a chroot the ESP checks cannot always see the partition type.
            sh.push("SYSTEMD_RELAX_ESP_CHECKS=1 kernel-install add-all".into());
        }
        ("fedora", _) => {
            // The kernel package's own kernel-install run leaves /boot empty when it runs in an
            // install root, so the first kernel goes in by hand. Later ones go through
            // kernel-install and the links hook.
            sh.push(each(
                "cp \"/lib/modules/$v/vmlinuz\" \"/boot/vmlinuz-$v\" && dracut --force --kver \"$v\" \"/boot/initramfs-$v.img\" \
                 && /etc/kernel/install.d/95-fspec-links.install add \"$v\"",
            ));
        }
        ("debian" | "ubuntu", Boot::Esp) => {
            sh.push(each(
                "SYSTEMD_RELAX_ESP_CHECKS=1 kernel-install add \"$v\" \"/boot/vmlinuz-$v\" \"/boot/initrd.img-$v\"",
            ));
        }
        ("debian" | "ubuntu", Boot::Grub) => sh.push(each("linux-update-symlinks install \"$v\" \"/boot/vmlinuz-$v\"")),
        // moss ships a prebuilt initramfs per kernel and syncs boot on every transaction.
        ("aerynos", _) => sh.push("rm -f /etc/machine-id\nsystemd-machine-id-setup".into()),
        ("void", _) => {
            if luks || boot == Boot::Zbm {
                sh.push("dracut --regenerate-all --force".into());
            }
            match boot {
                Boot::Esp => sh.push(each("/etc/kernel.d/post-install/60-fspec linux \"$v\"")),
                Boot::Grub => sh.push(each("/etc/kernel.d/post-install/60-fspec-links linux \"$v\"")),
                Boot::Zbm => {}
            }
        }
        _ => {}
    }
    sh.join("\n") + "\n"
}

/// Why `distro` cannot be switched to on this layout, with the fix, or `None` when it fits.
pub fn misfit(r: &Recipe, layout: &Layout, firmware: &Firmware) -> Option<String> {
    let mib = |b: u64| b / crate::machine::MIB;
    let disk_fix = "run `fspec disk plan` for the steps";
    match layout {
        Layout::Unsupported { reasons } => {
            Some(format!("this disk is not on a supported layout: {}; {disk_fix}", reasons.join("; ")))
        }
        Layout::Default { loader: Some(_), .. } if r.distro == "aerynos" => Some(
            "AerynOS's blsforme puts its kernels on the ESP, and beside Windows the ESP is Windows' own small one"
                .into(),
        ),
        Layout::Default { esp, .. } => {
            if mib(esp.size) < r.min_esp_mib {
                return Some(format!(
                    "the ESP is {} MiB and {} needs {} MiB; run `fspec disk migrate` to move it to a 2 GiB one",
                    mib(esp.size),
                    r.distro,
                    r.min_esp_mib
                ));
            }
            match esp.avail.map(mib) {
                Some(free) if free < r.kernel_mib => Some(format!(
                    "the ESP has {free} MiB free and {}'s kernels take about {} MiB; forget a root you no longer \
                     use, or run `fspec disk migrate` to move to a 2 GiB ESP",
                    r.distro, r.kernel_mib
                )),
                _ => None,
            }
        }
        Layout::Bios { .. } | Layout::Zfs { .. } if r.only_esp => {
            Some(format!("{} boots only through systemd-boot from the default layout", r.distro))
        }
        Layout::Bios { boot, .. } => match boot.avail.map(mib) {
            Some(free) if free < r.kernel_mib => Some(format!(
                "the shared boot partition has {free} MiB free and {}'s kernels take about {} MiB; forget a root \
                 you no longer use",
                r.distro, r.kernel_mib
            )),
            _ => None,
        },
        Layout::Zfs { .. } if !r.zfs => Some(format!(
            "{} ships ZFS only from a third-party repository, which fspec does not set up; keep it on a btrfs disk",
            r.distro
        )),
        Layout::Zfs { .. } if *firmware == Firmware::Bios => Some("ZFSBootMenu needs EFI here".into()),
        Layout::Zfs { .. } => None,
    }
}

/// What the layout rules out for `distro`, shown by `plan` but not refused.
pub fn caveats(distro: &str, layout: &Layout) -> Vec<String> {
    match layout {
        Layout::Zfs { .. } if distro != "ubuntu" => vec![format!(
            "out-of-tree ZFS modules are not signed by {distro}'s keys (Ubuntu's are the exception), so Secure Boot \
             and EAC Linux v2 do not work with {distro} on ZFS"
        )],
        Layout::Bios { .. } => {
            vec!["BIOS boot has no Secure Boot, so EAC Linux v2 cannot work on this machine".into()]
        }
        _ => Vec::new(),
    }
}
