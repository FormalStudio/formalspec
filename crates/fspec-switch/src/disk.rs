//! `fspec disk`: put a disk onto one of the layouts `switch --distro` installs roots on, from
//! scratch (`init`) or in place (`migrate`).

use std::path::Path;

use anyhow::{Context, Result, bail};
use fspec_core::resource::System;
use fspec_core::runner::{Cmd, Runner};
use fspec_core::{Host, Paths};

use crate::classic::{Sources, Target, create_root, fstab, grub_menu, install_root, kernel_options, s, sd_default};
use crate::machine::{
    BTRFS_OPTS, Btrfs, ESP_TYPE, Firmware, GIB, Layout, MIB, Machine, Part, SHARED_BOOT, SHARED_SUBVOLS, probe,
};
use crate::recipe::{Boot, boot_files, populate, recipe, upgrade};
use crate::windows::{self, BesideWindows, XBOOTLDR_SIZE};
use crate::{Plan, subvol};

/// The ESP size the default layout gives a disk.
pub const ESP_SIZE: u64 = 2 * GIB;
/// The shared boot partition of the BIOS layout.
pub const BOOT_SIZE: u64 = 2 * GIB;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Default,
    Zfs,
    Bios,
}

/// Identifiers chosen before formatting, so the whole plan, `fstab` included, is known up front.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ids {
    /// The btrfs filesystem UUID.
    pub fs: String,
    pub luks: String,
    /// FAT volume ID, 8 hex digits.
    pub serial: String,
    /// The ext4 UUID of the BIOS layout's boot partition.
    pub boot: String,
}

impl Ids {
    /// The FAT volume ID as blkid and `fstab` spell it.
    pub fn esp_uuid(&self) -> String {
        format!("{}-{}", &self.serial[..4], &self.serial[4..])
    }
}

/// `/dev/sda` partition 2 is `/dev/sda2`, `/dev/nvme0n1` partition 2 is `/dev/nvme0n1p2`.
pub fn partition(disk: &str, n: u64) -> String {
    if disk.ends_with(|c: char| c.is_ascii_digit()) { format!("{disk}p{n}") } else { format!("{disk}{n}") }
}

pub struct Init<'a> {
    pub device: &'a str,
    pub kind: Kind,
    pub luks: bool,
    pub ids: &'a Ids,
    pub swap_gib: u64,
    /// The running system booted through EFI, so bootctl and efibootmgr can write boot entries.
    pub efi_vars: bool,
    /// The first root to install, as `switch --distro` would.
    pub target: Option<&'a Target<'a>>,
    /// Kernel options to carry over, like `console=`.
    pub cmdline: &'a str,
    /// Run after the target's root is installed and before anything is unmounted, with the root
    /// mounted at [`root_dir`]. Ignored without a target.
    pub in_root: &'a [Cmd],
    /// The LUKS passphrase, passed on stdin. Without one, cryptsetup asks on the terminal.
    pub passphrase: Option<&'a str>,
    /// Install into space beside Windows instead of erasing the disk. Only the default layout.
    pub windows: Option<&'a BesideWindows>,
}

/// Where `init` mounts the root of `distro` while it installs it.
pub fn root_dir(paths: &Paths, distro: &str) -> String {
    s(&paths.system(format!("{DISK}/{distro}")))
}

/// The new root `switch --distro` or `disk init --distro` builds from the host's declaration.
pub fn target<'a>(
    host: &'a Host,
    distro: &'a str,
    release: Option<&'a str>,
    spec_dir: &'a Path,
    spec_file: &'a str,
    exe: &'a Path,
) -> Result<Target<'a>> {
    match &host.system {
        Some(System::Distro(d)) if d.distro == distro => {}
        _ => bail!(
            "host {} does not declare {distro} as its system. Edit `system =` to {distro}(...) first: the new root \
             is built from that block",
            host.name
        ),
    }
    let arch = host.platform.split('-').next().unwrap_or_default();
    Ok(Target {
        distro,
        host: &host.name,
        arch,
        release,
        spec_dir,
        spec_file,
        exe,
        before_apply: &[],
        skip_undecryptable_secrets: false,
    })
}

/// A random UUID from the kernel.
fn new_uuid() -> Result<String> {
    Ok(std::fs::read_to_string("/proc/sys/kernel/random/uuid").context("reading a random UUID")?.trim().to_string())
}

/// Fresh identifiers for the filesystems `init` creates.
pub fn new_ids() -> Result<Ids> {
    let serial = new_uuid()?.replace('-', "")[..8].to_uppercase();
    Ok(Ids { fs: new_uuid()?, luks: new_uuid()?, serial, boot: new_uuid()? })
}

/// Check that `device` is a whole disk with nothing mounted and room for a layout, and answer
/// whether it holds anything at all.
pub fn blank_disk(runner: &dyn Runner, device: &str) -> Result<bool> {
    let out =
        runner.check(&Cmd::new("lsblk").args(["-J", "-b", "-p", "-o", "NAME,TYPE,FSTYPE,SIZE,MOUNTPOINTS", device]))?;
    let json: serde_json::Value = serde_json::from_str(&out.stdout)?;
    let disk = &json["blockdevices"][0];
    if disk["type"].as_str() != Some("disk") {
        bail!("{device} is not a whole disk");
    }
    fn mounted(v: &serde_json::Value) -> bool {
        v["mountpoints"].as_array().is_some_and(|m| m.iter().any(|x| x.is_string()))
            || v["children"].as_array().is_some_and(|c| c.iter().any(mounted))
    }
    if mounted(disk) {
        bail!("{device} has mounted filesystems; unmount them first");
    }
    if disk["size"].as_u64().unwrap_or_default() < 16 << 30 {
        bail!("{device} is smaller than 16 GiB, too small for a 2 GiB ESP and a root");
    }
    Ok(disk["children"].as_array().is_none_or(|c| c.is_empty()) && disk["fstype"].is_null())
}

const DISK: &str = "/run/fspec/disk";

/// Partition and format `i.device` from scratch with the layout `i.kind`, create the shared
/// subvolumes or datasets, install the bootloader, and with a target install its root.
pub fn init(paths: &Paths, i: &Init) -> Result<Plan> {
    if i.luks && i.kind == Kind::Zfs {
        bail!("--luks does not apply to the ZFS layout; ZFS encrypts datasets itself");
    }
    let dir = |x: &str| s(&paths.system(format!("{DISK}/{x}")));
    let (top, efi, boot, zfs, loader) = (dir("top"), dir("efi"), dir("boot"), dir("zfs"), dir("esp"));
    let dev = i.device;
    let ids = i.ids;
    let mut plan = Plan::default();
    let mut cleanup = Vec::new();

    if i.windows.is_some() && i.kind != Kind::Default {
        bail!("installing beside Windows needs the default layout");
    }
    let mut esp_n = 1;
    let mut new_parts = Vec::new();
    if i.windows.is_none() {
        plan.run(Cmd::new("wipefs").args(["-a", dev]));
        plan.run(Cmd::new("sgdisk").args(["--zap-all", dev]));
    }
    let (data, layout) = match i.kind {
        Kind::Default if i.windows.is_some() => {
            let w = i.windows.expect("checked above");
            let (b, r) = windows::partitions(&mut plan, w, if i.luks { "8309" } else { "8300" })?;
            esp_n = b;
            new_parts = vec![b, r];
            (partition(dev, r), Kind::Default)
        }
        Kind::Default | Kind::Zfs => {
            let code = if i.kind == Kind::Zfs {
                "bf00"
            } else if i.luks {
                "8309"
            } else {
                "8300"
            };
            plan.run(
                Cmd::new("sgdisk")
                    .args(["-n1:0:+2G", "-t1:ef00", "-c1:EFI", "-n2:0:0"])
                    .arg(format!("-t2:{code}"))
                    .args(["-c2:root", dev]),
            );
            (partition(dev, 2), i.kind)
        }
        Kind::Bios => {
            let code = if i.luks { "8309" } else { "8300" };
            plan.run(
                Cmd::new("sgdisk")
                    .args(["-n1:0:+1M", "-t1:ef02", "-c1:bios", "-n2:0:+2G", "-t2:8300", "-c2:boot", "-n3:0:0"])
                    .arg(format!("-t3:{code}"))
                    .args(["-c3:root", dev]),
            );
            (partition(dev, 3), i.kind)
        }
    };
    plan.run(Cmd::new("udevadm").arg("settle"));
    // The new partitions start where the old ones did, or in space Windows used, so old
    // signatures are still there.
    if new_parts.is_empty() {
        new_parts = (1..=if i.kind == Kind::Bios { 3 } else { 2 }).collect();
    }
    plan.run(Cmd::new("wipefs").arg("-a").args(new_parts.iter().map(|&n| partition(dev, n))));

    let esp_part = match i.windows {
        Some(_) => Part {
            uuid: ids.esp_uuid(),
            parttype: crate::machine::XBOOTLDR_TYPE.into(),
            fstype: "vfat".into(),
            size: XBOOTLDR_SIZE,
            ..Part::default()
        },
        None => Part { uuid: ids.esp_uuid(), fstype: "vfat".into(), size: ESP_SIZE, ..Part::default() },
    };
    let boot_part = Part { uuid: ids.boot.clone(), fstype: "ext4".into(), size: BOOT_SIZE, ..Part::default() };
    if layout != Kind::Bios {
        let label = if i.windows.is_some() { "BOOT" } else { "EFI" };
        plan.run(Cmd::new("mkfs.vfat").args(["-F", "32", "-n", label, "-i", &ids.serial]).arg(partition(dev, esp_n)));
        plan.run(Cmd::new("mkdir").arg("-p").arg(&efi));
        plan.run(Cmd::new("mount").arg(partition(dev, esp_n)).arg(&efi));
        cleanup.push(Cmd::new("umount").arg(&efi));
    }
    if let Some(w) = i.windows {
        plan.run(Cmd::new("mkdir").arg("-p").arg(&loader));
        plan.run(Cmd::new("mount").arg(&w.disk.esp.dev).arg(&loader));
        cleanup.push(Cmd::new("umount").arg(&loader));
    }

    let layout = match layout {
        Kind::Zfs => {
            plan.expect(
                Cmd::new("zpool").args(["list", "-H", "rpool"]),
                false,
                "a pool named rpool is already imported; export it first",
            );
            // The pool records the creating host's ID, which every root's initramfs must match.
            plan.run(Cmd::new("sh").args(["-c", "test -e /etc/hostid || zgenhostid"]));
            plan.run(
                Cmd::new("zpool")
                    .args(["create", "-f", "-o", "ashift=12", "-O", "compression=zstd", "-O", "acltype=posixacl"])
                    .args(["-O", "xattr=sa", "-O", "relatime=on", "-O", "mountpoint=none", "-R", &zfs, "rpool", &data]),
            );
            cleanup.push(Cmd::new("zpool").args(["export", "rpool"]));
            plan.run(Cmd::new("zfs").args(["create", "-o", "mountpoint=none", "rpool/ROOT"]));
            plan.run(Cmd::new("zfs").args(["create", "-o", "mountpoint=/home", "rpool/home"]));
            plan.run(Cmd::new("zfs").args(["create", "-o", "mountpoint=/opt/fspec", "rpool/fspec"]));
            let zbm = format!("{efi}/EFI/zbm");
            plan.run(Cmd::new("mkdir").arg("-p").arg(&zbm).arg(format!("{efi}/EFI/BOOT")));
            plan.run(
                Cmd::new("curl")
                    .args(["-fsSL", "-o"])
                    .arg(format!("{zbm}/zfsbootmenu.efi"))
                    .arg("https://get.zfsbootmenu.org/efi"),
            );
            // The removable path boots it even where no firmware entry could be written.
            plan.run(Cmd::new("cp").arg(format!("{zbm}/zfsbootmenu.efi")).arg(format!("{efi}/EFI/BOOT/BOOTX64.EFI")));
            if i.efi_vars {
                plan.run(
                    Cmd::new("efibootmgr")
                        .args(["--create", "--disk", dev, "--part", "1", "--label", "ZFSBootMenu", "--loader"])
                        .arg("\\EFI\\zbm\\zfsbootmenu.efi"),
                );
            }
            Layout::Zfs { esp: esp_part, pool: "rpool".into() }
        }
        kind => {
            let mut fsdev = data.clone();
            let luks = i.luks.then(|| ids.luks.clone());
            if let Some(luks) = &luks {
                let name = format!("luks-{luks}");
                let format = Cmd::new("cryptsetup").args(["luksFormat", "--type", "luks2", "--uuid", luks]);
                let open = Cmd::new("cryptsetup").arg("open");
                // With --key-file=- cryptsetup takes all of stdin as the key, so both commands
                // get exactly the same bytes.
                let (format, open) = match i.passphrase {
                    Some(p) => {
                        (format.args(["--batch-mode", "--key-file=-"]).stdin(p), open.arg("--key-file=-").stdin(p))
                    }
                    None => (format.interactive(), open.interactive()),
                };
                plan.run(format.arg(&data));
                plan.run(open.args([&data, &name]));
                fsdev = format!("/dev/mapper/{name}");
            }
            if kind == Kind::Bios {
                plan.run(Cmd::new("mkfs.ext4").args(["-F", "-L", "boot", "-U", &ids.boot]).arg(partition(dev, 2)));
            }
            plan.run(Cmd::new("mkfs.btrfs").args(["-f", "-L", "root", "-U", &ids.fs, &fsdev]));
            plan.run(Cmd::new("mkdir").arg("-p").arg(&top));
            plan.run(Cmd::new("mount").args(["-o", BTRFS_OPTS]).arg(&fsdev).arg(&top));
            cleanup.push(Cmd::new("umount").arg(&top));
            if let Some(luks) = &luks {
                // Services socket-activated during the install can hold the filesystem in their own
                // mount namespaces for a while, so the mapping goes when the last of them lets go.
                cleanup.push(Cmd::new("cryptsetup").args(["close", "--deferred", &format!("luks-{luks}")]));
            }
            for v in std::iter::once("@home").chain(SHARED_SUBVOLS) {
                plan.run(Cmd::new("btrfs").args(["subvolume", "create"]).arg(format!("{top}/{v}")));
            }
            plan.run(swapfile(&top, i.swap_gib));
            let btrfs = Btrfs { uuid: ids.fs.clone(), luks, home: "/@home".into() };
            match kind {
                Kind::Bios => {
                    plan.run(Cmd::new("mkdir").arg("-p").arg(&boot));
                    plan.run(Cmd::new("mount").arg(partition(dev, 2)).arg(&boot));
                    cleanup.insert(0, Cmd::new("umount").arg(&boot));
                    plan.run(
                        Cmd::new("grub-install")
                            .arg("--target=i386-pc")
                            .arg(format!("--boot-directory={boot}"))
                            .arg(dev),
                    );
                    Layout::Bios { boot: boot_part, btrfs }
                }
                _ => {
                    if let Some(w) = i.windows {
                        windows::boot_manager(&mut plan, &w.disk, &loader, &efi, i.efi_vars);
                        Layout::Default { esp: esp_part, btrfs, loader: Some(Box::new(w.disk.esp.clone())) }
                    } else {
                        let mut bootctl = Cmd::new("bootctl").arg(format!("--esp-path={efi}")).arg("install");
                        if !i.efi_vars {
                            bootctl = bootctl.arg("--no-variables");
                        }
                        plan.run(bootctl);
                        Layout::Default { esp: esp_part, btrfs, loader: None }
                    }
                }
            }
        }
    };

    if let Some(t) = i.target {
        let firmware = if i.kind == Kind::Bios { Firmware::Bios } else { Firmware::Efi };
        if let Some(why) = crate::recipe::misfit(recipe(t.distro)?, &layout, &firmware) {
            bail!("cannot install {} on this layout: {why}", t.distro);
        }
        let root = root_dir(paths, t.distro);
        let mut roots = Plan::default();
        create_root(&mut roots, &layout, t.distro, &top, &root);
        let src = Sources {
            home: if i.kind == Kind::Zfs { format!("{zfs}/home") } else { format!("{top}/@home") },
            fspec: if i.kind == Kind::Zfs { format!("{zfs}/opt/fspec") } else { format!("{top}/@fspec") },
            esp: efi.clone(),
            shared_boot: boot.clone(),
            ..Sources::running(paths)
        };
        install_root(&mut roots, &layout, t, &root, &src, i.cmdline)?;
        plan.steps.extend(roots.steps);
        i.in_root.iter().for_each(|c| plan.run(c.clone()));
        cleanup.splice(0..0, roots.cleanup);
        match &layout {
            Layout::Default { loader: l, .. } => {
                let conf = if l.is_some() { &loader } else { &efi };
                sd_default(conf, t.distro).into_iter().for_each(|c| plan.run(c))
            }
            Layout::Zfs { pool, .. } => {
                plan.run(Cmd::new("zpool").arg("set").arg(format!("bootfs={pool}/ROOT/{}", t.distro)).arg(pool))
            }
            Layout::Bios { boot: b, .. } => plan.run(grub_menu(&boot, &b.uuid, t.distro)),
            Layout::Unsupported { .. } => {}
        }
    }
    plan.cleanup = cleanup;
    Ok(plan.private(&s(&paths.system(DISK))))
}

fn swapfile(top: &str, gib: u64) -> Cmd {
    Cmd::new("btrfs")
        .args(["filesystem", "mkswapfile", "--size"])
        .arg(format!("{gib}g"))
        .arg(format!("{top}/@swap/swapfile"))
}

/// One step of `fspec disk migrate`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub what: String,
    /// Asks for the device name and a recent backup before it runs.
    pub destructive: bool,
    pub plan: Plan,
}

/// What `disk migrate` reads beyond [`Machine`]: leftovers it can only see on the btrfs top level.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Leftovers {
    /// The running root moved into `@<distro>` and the old one is still there: `""` for the top
    /// level itself, or the old subvolume's name.
    pub old_root: Option<String>,
    /// `/home` and `/opt/fspec` were copied into their subvolumes and the originals still sit
    /// under the mounts in this root.
    pub shadowed: Vec<String>,
    /// The root's old `/swapfile` is still in `@<distro>` and not in use; the swapfile lives in
    /// `@swap` now.
    pub old_swapfile: bool,
}

/// Read the [`Leftovers`] by mounting the btrfs top level read-only.
pub fn leftovers(runner: &dyn Runner, paths: &Paths, m: &Machine) -> Result<Leftovers> {
    if m.fstype != "btrfs" || m.root != format!("/@{}", m.running) {
        return Ok(Leftovers::default());
    }
    let top = s(&paths.system("/run/fspec/disk/look"));
    runner.check(&Cmd::new("mkdir").arg("-p").arg(&top))?;
    runner.check(&Cmd::new("mount").args(["-o", "ro,subvolid=5"]).arg(format!("UUID={}", m.fs_uuid)).arg(&top))?;
    let read = || -> Result<Leftovers> {
        let mut found = Leftovers::default();
        // NixOS's /etc/os-release is a symlink into its store, which only resolves from that root.
        let has = |p: &str| -> Result<bool> {
            let test = Cmd::new("sh").args(["-c", r#"test -e "$1" || test -L "$1""#, "sh"]);
            Ok(probe(runner, test.arg(format!("{top}/{p}")))?.is_some())
        };
        if has("etc/os-release")? {
            found.old_root = Some(String::new());
        } else {
            for v in &m.facts.volumes {
                if !v.starts_with('@') && has(&format!("{v}/etc/os-release"))? {
                    found.old_root = Some(v.clone());
                }
            }
        }
        for dir in ["home", "opt/fspec"] {
            let listing = probe(runner, Cmd::new("ls").arg("-A").arg(format!("{top}/@{}/{dir}", m.running)))?;
            if listing.is_some_and(|l| !l.is_empty()) {
                found.shadowed.push(format!("/{dir}"));
            }
        }
        let swapfile = format!("{top}/@{}/swapfile", m.running);
        let swaps = std::fs::read_to_string(paths.system("/proc/swaps")).unwrap_or_default();
        found.old_swapfile = m.facts.volumes.iter().any(|v| v == "@swap")
            && probe(runner, Cmd::new("test").arg("-f").arg(&swapfile))?.is_some()
            && !swaps.lines().any(|l| l.split_whitespace().next() == Some("/swapfile"));
        Ok(found)
    };
    let found = read();
    runner.run(&Cmd::new("umount").arg(&top))?;
    found
}

/// The steps that bring the running system onto a supported layout, in order. `ids` names the
/// filesystems a step creates.
pub fn migrate(paths: &Paths, m: &Machine, left: &Leftovers, ids: &Ids, swap_gib: u64) -> Result<Vec<Step>> {
    match m.fstype.as_str() {
        "btrfs" => {}
        "ext4" => bail!(
            "the root filesystem is ext4, which converts to btrfs only while unmounted: boot a live system and run \
             `fspec disk migrate {}`",
            m.root_part.dev
        ),
        "zfs" => match &m.layout {
            Layout::Unsupported { reasons } => {
                bail!("fspec does not migrate within ZFS; fix these by hand: {}", reasons.join("; "))
            }
            _ => return Ok(Vec::new()),
        },
        other => bail!("the root filesystem is {other}; only ext4 converts to btrfs in place"),
    }
    let d = m.running.as_str();
    if d.is_empty() {
        bail!("could not read ID= from /etc/os-release, which names this root's subvolume");
    }
    let top = s(&paths.system("/run/fspec/disk/top"));
    // NixOS boots the way its own configuration says, so fspec only cleans up after
    // `disk migrate <device>` moved it into @nixos.
    if d == "nixos" {
        return Ok(private(paths, left_behind(paths, m, left, &top)));
    }
    recipe(d)?;
    let mount_top = |plan: &mut Plan| mount_top(plan, &top, format!("UUID={}", m.fs_uuid));
    let mut steps = Vec::new();

    // The shared subvolumes, with copies of what this root holds at their mount points. A
    // reflink copy shares the blocks, so it is quick and takes no room.
    let f = &m.facts;
    let home_subvol = !f.home.is_empty() && f.home != m.root;
    let missing: Vec<&str> = SHARED_SUBVOLS.iter().copied().filter(|v| !f.volumes.iter().any(|x| x == v)).collect();
    if !home_subvol || !missing.is_empty() {
        let mut plan = Plan::default();
        mount_top(&mut plan);
        if !home_subvol {
            plan.run(Cmd::new("btrfs").args(["subvolume", "create"]).arg(format!("{top}/@home")));
            plan.run(reflink_copy(&s(&paths.system("/home")), &format!("{top}/@home")));
        }
        for v in &missing {
            plan.run(Cmd::new("btrfs").args(["subvolume", "create"]).arg(format!("{top}/{v}")));
        }
        if missing.contains(&"@swap") {
            plan.run(swapfile(&top, swap_gib));
        }
        if missing.contains(&"@fspec") {
            plan.run(reflink_copy(&s(&paths.system("/opt/fspec")), &format!("{top}/@fspec")));
        }
        let what = format!(
            "create the shared subvolumes{}{}",
            if missing.is_empty() { String::new() } else { format!(" {}", missing.join(", ")) },
            if home_subvol { "" } else { " and @home, copying /home into it" }
        );
        steps.push(Step { what, destructive: false, plan });
    }
    let home = if home_subvol { f.home.clone() } else { "/@home".into() };
    let btrfs = Btrfs { uuid: m.fs_uuid.clone(), luks: m.luks.clone(), home };

    // The boot partition the layout needs, carved from the end of the btrfs partition when the
    // disk has none big enough.
    let target = match m.firmware {
        Firmware::Efi => {
            let esp = m.parts.iter().filter(|p| p.parttype == ESP_TYPE).max_by_key(|p| p.size);
            // A 2 GiB ESP an earlier run created but did not finish has no filesystem yet.
            let made = esp.filter(|e| e.size >= ESP_SIZE && e.fstype.is_empty() && e.label == "EFI");
            let new = match esp {
                Some(e) if e.size >= ESP_SIZE && made.is_none() => {
                    if !e.mounts.iter().any(|x| x == "/efi") {
                        let mut plan = Plan::default();
                        let efi = s(&paths.system("/efi"));
                        for old in &e.mounts {
                            plan.run(Cmd::new("umount").arg(s(&paths.system(old))));
                        }
                        plan.run(Cmd::new("mkdir").arg("-p").arg(&efi));
                        plan.run(Cmd::new("mount").arg(&e.dev).arg(&efi));
                        steps.push(Step { what: format!("mount the ESP {} at /efi", e.dev), destructive: false, plan });
                    }
                    e.clone()
                }
                _ => {
                    let old = m
                        .parts
                        .iter()
                        .filter(|p| p.parttype == ESP_TYPE && !p.fstype.is_empty())
                        .max_by_key(|p| p.size);
                    let (step, part) = carve_esp(paths, m, old, made, ids)?;
                    steps.push(step);
                    part
                }
            };
            Layout::Default { esp: new, btrfs, loader: None }
        }
        Firmware::Bios => {
            let gpt_without_grub = m.parts.iter().any(|p| !p.partuuid.is_empty())
                && !m.parts.iter().any(|p| p.parttype == crate::machine::BIOS_GRUB_TYPE);
            if gpt_without_grub {
                bail!(
                    "the GPT disk has no bios_grub partition, which GRUB needs to boot through BIOS; fspec does not add one"
                );
            }
            let boot = match m.parts.iter().find(|p| p.mounts.iter().any(|x| x == SHARED_BOOT)) {
                Some(b) => b.clone(),
                None => {
                    let made = m.parts.iter().find(|p| p.label == "boot" && p.fstype.is_empty());
                    let (step, part) = carve_boot(paths, m, made, ids)?;
                    steps.push(step);
                    part
                }
            };
            Layout::Bios { boot, btrfs }
        }
    };

    // This root moves into @<distro>, and its kernels go where the layout boots them.
    let own = format!("/@{d}");
    let needs_root = m.root != own || matches!(m.layout, Layout::Unsupported { .. });
    if needs_root {
        let mut plan = Plan::default();
        mount_top(&mut plan);
        let new = format!("{top}/{}", subvol(d));
        if m.root != own {
            let from = if m.root == "/" { top.clone() } else { format!("{top}{}", m.root) };
            plan.expect(
                Cmd::new("test").arg("-e").arg(&new),
                false,
                format!("{} already exists; rename or delete it first", subvol(d)),
            );
            plan.run(Cmd::new("btrfs").args(["subvolume", "snapshot", &from, &new]));
        }
        boot_setup(paths, &mut plan, m, &target, &new)?;
        let what = if m.root == own {
            format!("set up {d}'s boot on the {} layout", target.name())
        } else {
            format!("copy this root into {} as a snapshot and set up its boot there", subvol(d))
        };
        steps.push(Step { what, destructive: false, plan });
        return Ok(private(paths, steps));
    }

    Ok(private(paths, left_behind(paths, m, left, &top)))
}

fn mount_top(plan: &mut Plan, top: &str, source: String) {
    plan.run(Cmd::new("mkdir").arg("-p").arg(top));
    plan.run(Cmd::new("mount").arg("-o").arg(format!("subvolid=5,{BTRFS_OPTS}")).arg(source).arg(top));
    plan.cleanup.push(Cmd::new("umount").arg(top));
}

/// After a reboot into `@<distro>`, what the move left behind.
fn left_behind(paths: &Paths, m: &Machine, left: &Leftovers, top: &str) -> Vec<Step> {
    let d = m.running.as_str();
    let source = || format!("UUID={}", m.fs_uuid);
    let mut steps = Vec::new();
    if let Some(old) = &left.old_root {
        let mut plan = Plan::default();
        mount_top(&mut plan, top, source());
        let what = if old.is_empty() {
            if let Some(esp) = m.parts.iter().filter(|p| p.parttype == ESP_TYPE).find_map(|p| p.mounts.first()) {
                plan.expect(
                    Cmd::new("sh").args([
                        "-c",
                        r#"for f in "$1"/loader/entries/fspec-live*.conf; do [ -e "$f" ] || continue; if grep -oE '(findiso|fspec\.overlay)=[^ ]+' "$f" | grep -qv '=/@fspec/'; then exit 1; fi; done"#,
                        "sh",
                    ]).arg(s(&paths.system(esp))),
                    true,
                    "the fspec-live boot entry still finds its ISO or overlay outside @fspec, in the old top-level files",
                );
            }
            // Everything at the top level except the subvolumes the layout uses. The old root's own
            // subvolumes (Arch's /var/lib/machines) go first, deepest first, since `rm` stays on
            // one filesystem.
            let script = r#"btrfs subvolume list "$1" | sed 's/.* path //' | grep -v '^@' | sort -r | while read -r v; do btrfs subvolume delete "$1/$v"; done
for e in "$1"/* "$1"/.[!.]*; do case "${e##*/}" in @*) ;; *) rm -rf --one-file-system "$e" ;; esac; done"#;
            plan.run(Cmd::new("sh").args(["-c", script, "sh", top]));
            if m.facts.volumes.iter().any(|v| v == "ext2_saved") {
                "delete the old root's files from the btrfs top level, and ext2_saved, after which `btrfs-convert -r` \
                 cannot go back to ext4"
                    .to_string()
            } else {
                "delete the old root's files from the btrfs top level".to_string()
            }
        } else {
            plan.run(Cmd::new("btrfs").args(["subvolume", "delete", "--recursive"]).arg(format!("{top}/{old}")));
            format!("delete the old root subvolume {old}")
        };
        steps.push(Step { what, destructive: true, plan });
    }
    if !left.shadowed.is_empty() {
        let mut plan = Plan::default();
        mount_top(&mut plan, top, source());
        for dir in &left.shadowed {
            let under = format!("{top}/@{d}{dir}");
            plan.run(Cmd::new("sh").args([
                "-c",
                r#"find "$1" -mindepth 1 -maxdepth 1 -exec rm -rf --one-file-system {} +"#,
                "sh",
                &under,
            ]));
        }
        let what = format!("delete the copies of {} left under their mounts in @{d}", left.shadowed.join(" and "));
        steps.push(Step { what, destructive: true, plan });
    }
    if left.old_swapfile {
        let mut plan = Plan::default();
        mount_top(&mut plan, top, source());
        plan.run(Cmd::new("rm").arg("-f").arg(format!("{top}/@{d}/swapfile")));
        steps.push(Step { what: format!("delete the old /swapfile in @{d}"), destructive: true, plan });
    }
    steps
}

fn private(paths: &Paths, steps: Vec<Step>) -> Vec<Step> {
    let dir = s(&paths.system(DISK));
    steps.into_iter().map(|st| Step { plan: st.plan.private(&dir), ..st }).collect()
}

fn reflink_copy(from: &str, to: &str) -> Cmd {
    Cmd::new("sh").args(["-c", r#"if [ -d "$1" ]; then cp -a --reflink=always "$1/." "$2/"; fi"#, "sh", from, to])
}

/// Shrink the btrfs partition from its end by `size` (plus alignment) and return the commands
/// and the first sector of the freed space.
fn shrink_btrfs(paths: &Paths, m: &Machine, size: u64, plan: &mut Plan) -> Result<u64> {
    if m.luks.is_some() {
        bail!(
            "the btrfs filesystem sits in LUKS, and fspec does not shrink a LUKS container; use `fspec disk init` on another disk"
        );
    }
    let p = &m.root_part;
    let sectors = |b: u64| b / 512;
    let room = size + 2 * MIB;
    if p.size < room + 8 * GIB {
        bail!("the btrfs partition {} is too small to give up {} MiB", p.dev, room / MIB);
    }
    let new_size = p.size - room;
    plan.run(
        Cmd::new("btrfs").args(["filesystem", "resize"]).arg(format!("{}", new_size - MIB)).arg(s(&paths.system("/"))),
    );
    plan.run(
        Cmd::new("sfdisk")
            .args(["--no-reread", "-N"])
            .arg(p.partn.to_string())
            .arg(&m.disk)
            .stdin(format!(", {}\n", sectors(new_size))),
    );
    plan.run(Cmd::new("partx").args(["-u", &m.disk]));
    // Align the new partition to 1 MiB.
    let end = p.start + sectors(new_size);
    Ok(end.div_ceil(2048) * 2048)
}

fn next_partn(m: &Machine) -> u64 {
    m.parts.iter().map(|p| p.partn).max().unwrap_or_default() + 1
}

/// The partition a carve step makes: `made` when an earlier run already shrank the btrfs
/// partition and created it, else a new one at the end of the disk.
fn carved(
    paths: &Paths,
    m: &Machine,
    made: Option<&Part>,
    code: &str,
    label: &str,
    plan: &mut Plan,
) -> Result<(String, u64)> {
    if let Some(p) = made {
        return Ok((p.dev.clone(), p.partn));
    }
    let start = shrink_btrfs(paths, m, ESP_SIZE, plan)?;
    let n = next_partn(m);
    plan.run(
        Cmd::new("sgdisk")
            .arg(format!("-n{n}:{start}:+2G"))
            .arg(format!("-t{n}:{code}"))
            .arg(format!("-c{n}:{label}"))
            .arg(&m.disk),
    );
    // Adding every partition fails on the ones the kernel already has.
    plan.run(Cmd::new("partx").args(["-a", "-n"]).arg(format!("{n}:{n}")).arg(&m.disk));
    plan.run(Cmd::new("udevadm").arg("settle"));
    Ok((partition(&m.disk, n), n))
}

/// Carve a 2 GiB ESP out of the btrfs partition and copy the old one into it. The new ESP takes
/// the old one's partition GUID and FAT serial, so firmware entries and every `fstab` follow it;
/// the old one is retyped as Linux data with new identifiers and stays as a copy.
fn carve_esp(paths: &Paths, m: &Machine, old: Option<&Part>, made: Option<&Part>, ids: &Ids) -> Result<(Step, Part)> {
    let mut plan = Plan::default();
    let (dev, n) = carved(paths, m, made, "ef00", "EFI", &mut plan)?;
    let serial = old.map(|o| o.uuid.replace('-', "")).unwrap_or_else(|| ids.serial.clone());
    let new_mnt = s(&paths.system(format!("{DISK}/esp")));
    let efi = s(&paths.system("/efi"));
    plan.run(Cmd::new("mkfs.vfat").args(["-F", "32", "-n", "EFI", "-i", &serial, &dev]));
    if let Some(o) = old {
        plan.run(Cmd::new("mkdir").arg("-p").arg(&new_mnt));
        plan.run(Cmd::new("mount").arg(&dev).arg(&new_mnt));
        let src = match o.mounts.first() {
            Some(at) => s(&paths.system(at)),
            None => {
                let tmp = s(&paths.system(format!("{DISK}/oldesp")));
                plan.run(Cmd::new("mkdir").arg("-p").arg(&tmp));
                plan.run(Cmd::new("mount").arg(&o.dev).arg(&tmp));
                tmp
            }
        };
        plan.run(Cmd::new("cp").arg("-a").arg(format!("{src}/.")).arg(format!("{new_mnt}/")));
        plan.run(Cmd::new("umount").arg(&new_mnt));
        plan.run(Cmd::new("umount").arg(&src));
        for extra in o.mounts.iter().skip(1) {
            plan.run(Cmd::new("umount").arg(s(&paths.system(extra))));
        }
        plan.run(Cmd::new("fatlabel").args(["-r", "-i", &o.dev]));
        plan.run(
            Cmd::new("sgdisk")
                .arg(format!("-u{}:R", o.partn))
                .arg(format!("-t{}:8300", o.partn))
                .arg(format!("-c{}:old-esp", o.partn))
                .arg(&m.disk),
        );
        plan.run(Cmd::new("sgdisk").arg(format!("-u{n}:{}", o.partuuid)).arg(&m.disk));
        plan.run(Cmd::new("partx").args(["-u", &m.disk]));
        plan.run(Cmd::new("udevadm").arg("settle"));
    }
    plan.run(Cmd::new("mkdir").arg("-p").arg(&efi));
    plan.run(Cmd::new("mount").arg(&dev).arg(&efi));
    let shrink =
        if made.is_some() { "format".to_string() } else { "shrink the btrfs partition by 2 GiB and create".into() };
    let what = match old {
        Some(o) => format!("{shrink} a 2 GiB ESP at {dev} and move the {} MiB ESP {} into it", o.size / MIB, o.dev),
        None => format!("{shrink} a 2 GiB ESP at {dev}"),
    };
    let uuid = format!("{}-{}", &serial[..4], &serial[4..]);
    let part = Part { dev, uuid, parttype: ESP_TYPE.into(), fstype: "vfat".into(), size: ESP_SIZE, ..Part::default() };
    Ok((Step { what, destructive: true, plan }, part))
}

/// Carve the BIOS layout's shared ext4 boot partition out of the btrfs partition.
fn carve_boot(paths: &Paths, m: &Machine, made: Option<&Part>, ids: &Ids) -> Result<(Step, Part)> {
    let mut plan = Plan::default();
    let (dev, _) = carved(paths, m, made, "8300", "boot", &mut plan)?;
    let shared = s(&paths.system(SHARED_BOOT));
    plan.run(Cmd::new("mkfs.ext4").args(["-F", "-L", "boot", "-U", &ids.boot, &dev]));
    plan.run(Cmd::new("mkdir").arg("-p").arg(&shared));
    plan.run(Cmd::new("mount").arg(&dev).arg(&shared));
    let what = match made {
        Some(_) => format!("format the shared boot partition at {dev}"),
        None => format!("shrink the btrfs partition by 2 GiB and create the shared boot partition at {dev}"),
    };
    let part = Part { dev, uuid: ids.boot.clone(), fstype: "ext4".into(), size: BOOT_SIZE, ..Part::default() };
    Ok((Step { what, destructive: true, plan }, part))
}

/// Make the running distro's root at `new` (the mounted `@<distro>` path) boot on `layout`: its
/// `fstab`, kernel tooling, kernels, and the bootloader with this root as the default.
fn boot_setup(paths: &Paths, plan: &mut Plan, m: &Machine, layout: &Layout, new: &str) -> Result<()> {
    let d = m.running.as_str();
    let Some(boot) = Boot::of(layout) else { bail!("no layout to set up boot on") };
    let luks = m.luks.is_some();
    let src = Sources::running(paths);
    let root = s(&paths.system(format!("{DISK}/{d}")));
    plan.run(Cmd::new("mkdir").arg("-p").arg(&root));
    plan.run(Cmd::new("mount").arg("--bind").arg(new).arg(&root));
    plan.cleanup.insert(0, Cmd::new("umount").arg("-R").arg(&root));
    let options = kernel_options(layout, d, &m.cmdline);
    let tee = |plan: &mut Plan, path: String, text: String| {
        plan.run(Cmd::new("mkdir").arg("-p").arg(std::path::Path::new(&path).parent().map(s).unwrap_or_default()));
        plan.run(Cmd::new("tee").arg(path).stdin(text));
    };
    tee(plan, format!("{root}/etc/fstab"), fstab(layout, d));
    if let Some(l) = &m.luks {
        let opts = if matches!(d, "debian" | "ubuntu") { "luks,discard,initramfs" } else { "luks,discard" };
        tee(plan, format!("{root}/etc/crypttab"), format!("luks-{l} UUID={l} none {opts}\n"));
    }
    tee(plan, format!("{root}{}", crate::recipe::cmdline_file(d)), format!("{options}\n"));
    for f in boot_files(d, boot, luks) {
        tee(plan, format!("{root}{}", f.path), f.text);
        if f.exec {
            plan.run(Cmd::new("chmod").arg("755").arg(format!("{root}{}", f.path)));
        }
    }
    let shared = format!("{}/{d}", src.shared_boot);
    if boot == Boot::Grub {
        plan.run(Cmd::new("mkdir").arg("-p").arg(&shared));
        plan.run(Cmd::new("cp").arg("-a").arg(format!("{root}/boot/.")).arg(format!("{shared}/")));
        tee(plan, format!("{shared}/fspec-cmdline"), format!("{options}\n"));
    }
    let mut binds = vec![(src.home.clone(), "home"), ("/dev".into(), "dev"), ("/sys".into(), "sys")];
    match boot {
        Boot::Grub => binds.push((shared.clone(), "boot")),
        _ => binds.push((src.esp.clone(), "efi")),
    }
    for (from, to) in binds {
        let to = format!("{root}/{to}");
        plan.run(Cmd::new("mkdir").arg("-p").arg(&to));
        plan.run(Cmd::new("mount").arg("--rbind").arg(from).arg(&to));
        plan.run(Cmd::new("mount").arg("--make-rslave").arg(&to));
    }
    plan.run(Cmd::new("mount").args(["-t", "proc", "proc"]).arg(format!("{root}/proc")));
    let chroot = |cmd: &str| {
        Cmd::new("chroot")
            .arg(&root)
            .args(["sh", "-c", cmd])
            .env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
    };
    if let Some(install) = install_packages(d, boot, luks) {
        plan.run(chroot(&install));
    }
    if let Some(sh) = upgrade(d, std::env::consts::ARCH, boot, luks) {
        plan.run(chroot(&sh));
    }
    plan.run(chroot(&populate(d, boot, luks)));
    match layout {
        Layout::Default { .. } => {
            plan.run(Cmd::new("bootctl").arg(format!("--esp-path={}", src.esp)).arg("install"));
            sd_default(&src.esp, d).into_iter().for_each(|c| plan.run(c));
        }
        Layout::Bios { boot: b, .. } => {
            let install = if d == "fedora" { "grub2-install" } else { "grub-install" };
            plan.run(
                Cmd::new(install)
                    .arg("--target=i386-pc")
                    .arg(format!("--boot-directory={}", src.shared_boot))
                    .arg(&m.disk),
            );
            plan.run(grub_menu(&src.shared_boot, &b.uuid, d));
        }
        _ => {}
    }
    Ok(())
}

/// The running distro's own package manager installing the boot packages [`populate`] does not
/// install itself.
fn install_packages(distro: &str, boot: Boot, luks: bool) -> Option<String> {
    let pkgs = crate::recipe::boot_packages(distro, boot, luks).join(" ");
    if pkgs.is_empty() {
        return None;
    }
    Some(match distro {
        "arch" => format!("pacman -S --needed --noconfirm {pkgs}"),
        "fedora" => format!("dnf install -y {pkgs}"),
        "void" => format!("xbps-install -y {pkgs}"),
        _ => return None,
    })
}

/// The `sed` expressions that turn the ext4 root line of an `fstab` into a btrfs one: the type,
/// no fsck pass (btrfs has none), and without ext4's `errors=`, which btrfs refuses to mount with.
pub(crate) const FSTAB_SED: [&str; 3] = [
    r"s#^\([^[:space:]]\{1,\}[[:space:]]\{1,\}/[[:space:]]\{1,\}\)ext4\([[:space:]]\{1,\}[^[:space:]]\{1,\}[[:space:]]\{1,\}[^[:space:]]\{1,\}[[:space:]]\{1,\}\)[0-9]#\1btrfs\20#",
    r"/^[^[:space:]]\{1,\}[[:space:]]\{1,\}\/[[:space:]]\{1,\}btrfs[[:space:]]/s/,\{0,1\}errors=[^,[:space:]]*//",
    r"s#^\([^[:space:]]\{1,\}[[:space:]]\{1,\}/[[:space:]]\{1,\}btrfs[[:space:]]\{1,\}\),#\1#",
];

/// Shrinks the ext4 filesystem on `device` to a whole MiB. btrfs-convert (6.14) rounds the last
/// chunk up to the node size, so on a filesystem that ends off a 16 KiB boundary it writes a dev
/// extent past the end of the device and the kernel refuses to mount the result ("dev extent ...
/// is beyond device boundary"). At most 1 MiB comes off the end, which resize2fs moves data out of.
fn align_ext4(device: &str) -> Cmd {
    Cmd::new("sh").args([
        "-c",
        r#"set -e; h=$(dumpe2fs -h "$1" 2>/dev/null)
bs=$(printf '%s\n' "$h" | sed -n 's/^Block size: *//p'); n=$(printf '%s\n' "$h" | sed -n 's/^Block count: *//p')
a=$((n / (1048576 / bs) * (1048576 / bs))); [ "$a" -eq "$n" ] || resize2fs "$1" "$a""#,
        "sh",
        device,
    ])
}

/// The ext4 root partition `device` (unmounted) converted to btrfs in place, keeping its UUID, with
/// its `fstab` and initramfs updated and GRUB's config rewritten so it boots as before. `disk
/// migrate` then runs on the converted system for the rest.
pub fn convert(paths: &Paths, device: &str, disk: &str, firmware: &Firmware) -> Plan {
    let mnt = s(&paths.system(format!("{DISK}/convert")));
    let mut plan = Plan::default();
    plan.run(Cmd::new("e2fsck").args(["-fy", device]));
    plan.run(align_ext4(device));
    plan.run(Cmd::new("btrfs-convert").args(["--uuid", "copy", device]));
    plan.run(Cmd::new("mkdir").arg("-p").arg(&mnt));
    plan.run(Cmd::new("mount").arg(device).arg(&mnt));
    plan.cleanup.push(Cmd::new("umount").arg("-R").arg(&mnt));
    plan.run(Cmd::new("sed").arg("-i").args(FSTAB_SED.iter().flat_map(|e| ["-e", e])).arg(format!("{mnt}/etc/fstab")));
    for (from, to) in [("/dev", "dev"), ("/sys", "sys"), ("/proc", "proc")] {
        plan.run(Cmd::new("mount").arg("--rbind").arg(from).arg(format!("{mnt}/{to}")));
        plan.run(Cmd::new("mount").arg("--make-rslave").arg(format!("{mnt}/{to}")));
    }
    let grub = match firmware {
        Firmware::Bios => format!("grub-install --target=i386-pc {disk} || grub2-install --target=i386-pc {disk}; "),
        Firmware::Efi => String::new(),
    };
    let script = format!(
        "mount -a -O no_netdev 2>/dev/null || true\n\
         if ! command -v btrfs >/dev/null; then\n\
         if command -v apt-get >/dev/null; then apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y btrfs-progs\n\
         elif command -v dnf >/dev/null; then dnf install -y btrfs-progs\n\
         elif command -v pacman >/dev/null; then pacman -S --noconfirm btrfs-progs\n\
         else xbps-install -Sy btrfs-progs; fi; fi\n\
         if command -v mkinitcpio >/dev/null; then mkinitcpio -P\n\
         elif command -v update-initramfs >/dev/null; then update-initramfs -u -k all\n\
         else dracut --regenerate-all --force; fi\n\
         {grub}if command -v grub-mkconfig >/dev/null; then grub-mkconfig -o /boot/grub/grub.cfg\n\
         else grub2-mkconfig -o /boot/grub2/grub.cfg; fi\n\
         umount -a -t vfat 2>/dev/null || true\n"
    );
    // The root's own resolv.conf can point at a resolver that is not running here; it goes back
    // afterwards, whether or not the chroot succeeded.
    let src = Sources::running(paths);
    let resolv = format!("{mnt}/etc/resolv.conf");
    plan.run(Cmd::new("sh").args([
        "-c",
        r#"mv "$3" "$3.fspec" 2>/dev/null || true; src=$1; if grep -qs '^nameserver 127\.0\.0\.53' "$src" && [ -s "$2" ]; then src=$2; fi; cp -L "$src" "$3""#,
        "sh",
        &src.resolv,
        &src.resolved,
        &resolv,
    ]));
    plan.cleanup.insert(
        0,
        Cmd::new("sh").args([
            "-c",
            r#"if [ -e "$1.fspec" ] || [ -L "$1.fspec" ]; then mv -f "$1.fspec" "$1"; fi"#,
            "sh",
            &resolv,
        ]),
    );
    plan.run(
        Cmd::new("chroot")
            .arg(&mnt)
            .args(["sh", "-ec", &script])
            .env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"),
    );
    plan.private(&s(&paths.system(DISK)))
}

/// A NixOS root on the unmounted ext4 partition `disk migrate <device>` converts, and the system
/// to boot from `@nixos` afterwards.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NixosRoot {
    pub device: String,
    /// The generation `/nix/var/nix/profiles/system` points at.
    pub generation: u64,
    /// A store path in the root's own store whose `fileSystems` mount `/` from `@nixos`.
    pub system: String,
    /// The ESP as the system's `fstab` names it, and where it mounts it.
    pub esp: String,
    pub esp_at: String,
}

/// `path` inside the tree at `root`, following symlinks as if `root` were `/`.
pub(crate) fn resolve_in(root: &Path, path: &str) -> Result<std::path::PathBuf> {
    let parts = |p: &str| p.split('/').filter(|c| !c.is_empty()).rev().map(String::from).collect::<Vec<_>>();
    let mut todo = parts(path);
    let mut at = std::path::PathBuf::new();
    let mut hops = 0;
    while let Some(c) = todo.pop() {
        match c.as_str() {
            "." => continue,
            ".." => {
                at.pop();
                continue;
            }
            _ => {}
        }
        let next = at.join(&c);
        match std::fs::read_link(root.join(&next)) {
            Ok(target) => {
                hops += 1;
                if hops > 40 {
                    bail!("too many symlinks resolving {path} under {}", root.display());
                }
                let target = target.to_string_lossy().into_owned();
                if target.starts_with('/') {
                    at = std::path::PathBuf::new();
                }
                todo.extend(parts(&target));
            }
            Err(_) => at = next,
        }
    }
    Ok(root.join(at))
}

/// Check that the `fstab` of the system to boot mounts `/` from `@nixos` on the filesystem `uuid`
/// (and `/home` and `/swap`, if it mounts them, from `@home` and `@swap`), and return its ESP line's device and mount point.
pub fn nixos_fstab(fstab: &str, uuid: &str) -> Result<(String, String)> {
    let lines: Vec<Vec<&str>> = fstab
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>())
        .filter(|f| f.len() >= 4 && !f[0].starts_with('#'))
        .collect();
    let ours = |f: &[&str]| f[0] == format!("/dev/disk/by-uuid/{uuid}") || f[0] == format!("UUID={uuid}");
    let subvol = |f: &[&str], want: &str| {
        f[2] == "btrfs"
            && ours(f)
            && f[3].split(',').any(|o| o == format!("subvol={want}") || o == format!("subvol=/{want}"))
    };
    let Some(root) = lines.iter().find(|f| f[1] == "/") else { bail!("its fstab mounts nothing at /") };
    if !subvol(root, "@nixos") {
        bail!(
            "its fstab mounts / as `{}`, not btrfs subvol=@nixos on UUID {uuid}; build it with fileSystems.\"/\" on that",
            root.join(" ")
        );
    }
    for (at, want) in [("/home", "@home"), ("/swap", "@swap")] {
        if let Some(line) = lines.iter().find(|f| f[1] == at)
            && !subvol(line, want)
        {
            bail!("its fstab mounts {at} as `{}`, not btrfs subvol={want} on UUID {uuid}", line.join(" "));
        }
    }
    let Some(esp) = lines.iter().find(|f| f[2] == "vfat") else {
        bail!("its fstab mounts no vfat ESP, where its systemd-boot entries go")
    };
    Ok((esp[0].to_string(), esp[1].to_string()))
}

/// Read the root on the unmounted ext4 `device` (UUID `uuid`) read-only, and when it is NixOS,
/// check `system` against it. Refuses a NixOS root without a system to boot.
pub fn nixos_root(
    runner: &dyn Runner,
    paths: &Paths,
    device: &str,
    uuid: &str,
    system: Option<&str>,
) -> Result<Option<NixosRoot>> {
    let mnt = s(&paths.system(format!("{DISK}/look")));
    runner.check(&Cmd::new("mkdir").arg("-p").arg(&mnt))?;
    runner.check(&Cmd::new("mount").args(["-t", "ext4", "-o", "ro,noload", device, &mnt]))?;
    let found = nixos_at(Path::new(&mnt), device, uuid, system);
    runner.run(&Cmd::new("umount").arg(&mnt))?;
    found
}

/// The NixOS root of the ext4 `device` (UUID `uuid`) whose files are at `root`, with `system`
/// checked against it, or `None` when it is not NixOS.
pub fn nixos_at(root: &Path, device: &str, uuid: &str, system: Option<&str>) -> Result<Option<NixosRoot>> {
    let profile = root.join("nix/var/nix/profiles/system");
    if !root.join("etc/NIXOS").exists() && profile.symlink_metadata().is_err() {
        return Ok(None);
    }
    let Some(system) = system else {
        bail!(
            "{device} holds NixOS, whose fstab and initrd come from its configuration: build a system that \
             mounts / as btrfs subvol=@nixos, copy it into this root's /nix/store, and name it with --nixos-system"
        )
    };
    let link = std::fs::read_link(&profile).with_context(|| format!("reading {}", profile.display()))?;
    let link = link.to_string_lossy();
    let generation = link
        .strip_prefix("system-")
        .and_then(|l| l.strip_suffix("-link"))
        .and_then(|n| n.parse().ok())
        .with_context(|| format!("the system profile points at {link}, not a system-<n>-link generation"))?;
    let name = system.strip_prefix("/nix/store/").filter(|n| !n.is_empty() && !n.contains('/'));
    if name.is_none() || !root.join(&system[1..]).is_dir() {
        bail!("{system} is not a store path in {device}'s /nix/store; copy it there with `nix copy --to`");
    }
    if !resolve_in(root, &format!("{system}/bin/switch-to-configuration"))?.is_file() {
        bail!("{system} has no bin/switch-to-configuration, so it is not a NixOS system");
    }
    let fstab = resolve_in(root, &format!("{system}/etc/fstab"))?;
    let fstab = std::fs::read_to_string(&fstab).with_context(|| format!("reading {system}/etc/fstab"))?;
    let (esp, esp_at) = nixos_fstab(&fstab, uuid).with_context(|| format!("{system} does not fit"))?;
    Ok(Some(NixosRoot { device: device.into(), generation, system: system.into(), esp, esp_at }))
}

/// The ESP directories NixOS's systemd-boot installer rewrites, which a rollback puts back.
const ESP_DIRS: &str = "loader EFI/systemd EFI/BOOT";

/// Where `disk migrate <device>` mounts the disk's ESP on the live system. The journal of a NixOS
/// migration lives on it, so it survives the live system rebooting.
pub const ESP_MOUNT: &str = "/run/fspec/esp";

/// Mount the ESP of the disk holding `device` at [`ESP_MOUNT`], unless it is mounted there, and
/// return its device and mount point. `None` for a disk without one.
pub fn mount_esp(runner: &dyn Runner, paths: &Paths, device: &str) -> Result<Option<(String, String)>> {
    let disk = runner.check(&Cmd::new("lsblk").args(["-ndpo", "PKNAME", device]))?.stdout.trim().to_string();
    let parts = runner.check(&Cmd::new("lsblk").args(["-nrpo", "NAME,PARTTYPE", &disk]))?.stdout;
    let esps: Vec<&str> = parts
        .lines()
        .filter_map(|l| l.split_once(' '))
        .filter(|(_, t)| t.trim().eq_ignore_ascii_case(ESP_TYPE))
        .map(|(n, _)| n)
        .collect();
    let esp = match esps[..] {
        [] => return Ok(None),
        [one] => one.to_string(),
        _ => bail!("{disk} has more than one ESP: {}", esps.join(", ")),
    };
    let at = s(&paths.system(ESP_MOUNT));
    if probe(runner, Cmd::new("mountpoint").args(["-q", &at]))?.is_none() {
        // FAT has no journal, so a reset mid-migration can leave it needing a repair before the
        // journal on it is read.
        runner.check(&Cmd::new("sh").args(["-c", r#"fsck.fat -a -w "$1"; [ $? -le 1 ]"#, "sh", &esp]))?;
        runner.check(&Cmd::new("mkdir").arg("-p").arg(&at))?;
        runner.check(&Cmd::new("mount").arg(&esp).arg(&at))?;
    }
    Ok(Some((esp, at)))
}

/// `systemd.run=` alone only makes its unit the boot target through a generated `default.target`,
/// which the `default.target` that Debian and NixOS keep in `/etc/systemd/system` outranks, and
/// which would replace the normal boot anyway; this starts the unit beside it.
const RUN_BESIDE: &str = "systemd.wants=kernel-command-line.service";

/// What a migration appends to the options of the new NixOS entry while it is on trial.
const TRIAL: &str = "boot.panic_on_fail panic=10 systemd.mask=emergency.target systemd.mask=emergency.service \
                     systemd.mask=rescue.target systemd.mask=rescue.service";

/// Drop the trial arguments from the blessed entry that booted this system, on the ESP mounted at
/// `esp`, for an entry whose own `systemd.run=` did not, or one written before it had one. Refuses
/// while the booted entry still counts tries, since only a blessed boot may lose them.
pub fn unmask(paths: &Paths, esp: &str) -> Result<String> {
    let var = paths.system("/sys/firmware/efi/efivars/LoaderEntrySelected-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f");
    let raw =
        std::fs::read(&var).with_context(|| format!("reading {}; was this booted by systemd-boot?", var.display()))?;
    let utf16: Vec<u16> =
        raw.get(4..).unwrap_or_default().as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
    let id = String::from_utf16(&utf16)?.trim_end_matches('\0').to_string();
    let entries = paths.system(format!("{esp}/loader/entries"));
    let entry = entries.join(&id);
    if !entry.exists() {
        bail!(
            "{} is not there, so the boot of {id} is not blessed yet; run this after boot-complete.target",
            entry.display()
        );
    }
    let text = std::fs::read_to_string(&entry)?;
    let mut changed = false;
    let lines: Vec<String> = text
        .lines()
        .map(|l| {
            let run = l.find(" systemd.run=\"/bin/sh ").map(|i| &l[..i]).unwrap_or(l);
            match run.strip_suffix(&format!(" {TRIAL}")) {
                Some(kept) if l.starts_with("options ") => {
                    changed = true;
                    kept.to_string()
                }
                _ => l.to_string(),
            }
        })
        .collect();
    let script = paths.system(format!("{esp}/loader/fspec-unmask"));
    if !changed {
        return Ok(format!("{} has no trial arguments", entry.display()));
    }
    let tmp = entry.with_extension("conf.fspec-tmp");
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all((lines.join("\n") + "\n").as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, &entry)?;
    if script.exists() {
        std::fs::remove_file(&script)?;
    }
    // vfat does not persist a rename through an fsync of its directory.
    let synced = std::process::Command::new("sync").arg("-f").arg(&entries).status()?;
    if !synced.success() {
        bail!("sync -f {} failed", entries.display());
    }
    Ok(format!("{} boots without the trial arguments now", entry.display()))
}

/// The script a trial NixOS entry starts through `systemd.run=`: it hands itself to a transient
/// unit so the boot does not wait on it, then, once systemd-bless-boot has renamed the booted
/// entry to its name without a counter, drops `trial` from the entry's options line. A boot that
/// is never blessed leaves the entry as it is.
fn unmask_script(trial: &str) -> String {
    let trial = trial.replace('\'', r"'\''");
    format!(
        r#"PATH=/run/current-system/sw/bin:$PATH
[ "$1" = wait ] || exec systemd-run --no-block --collect --unit=fspec-unmask sh "$0" wait
id=$(tail -c +5 /sys/firmware/efi/efivars/LoaderEntrySelected-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f | tr -d '\0')
e="${{0%/*}}/entries/$id"
until [ -f "$e" ]; do sleep 5; done
trial=' {trial}'
while IFS= read -r l; do case $l in "options "*"$trial") l=${{l%"$trial"}};; esac; printf '%s\n' "$l"; done < "$e" > "$e.fspec-tmp"
sync "$e.fspec-tmp"; mv "$e.fspec-tmp" "$e"; rm -f "$0"; sync -f "$e""#
    )
}

/// The steps that move the NixOS root `r` onto btrfs, run from a live system with the ESP mounted
/// at `esp_mnt`: boot the fspec-live entry by default meanwhile, convert the root, keeping
/// ext2_saved, snapshot the top level into `@nixos` and add the shared subvolumes, then boot
/// `r.system` from `@nixos` as a new generation. Every step journals its undo: `btrfs-convert -r`
/// and the ESP's boot files saved under `state`, so a failure leaves NixOS on ext4 as it was.
pub fn nixos_migrate(paths: &Paths, r: &NixosRoot, esp_mnt: &str, state: &str) -> Vec<Step> {
    let dev = r.device.as_str();
    let disk = s(&paths.system(DISK));
    let top = s(&paths.system(format!("{DISK}/top")));
    let mut steps = Vec::new();

    // A live system that reboots mid-migration comes back into fspec-live, where the journal on
    // the ESP rolls the migration back. An EFI default entry would outrank loader.conf.
    let mut plan = Plan::default();
    plan.expect(
        Cmd::new("sh").args(["-c", r#"ls "$1"/loader/entries/fspec-live*.conf"#, "sh", esp_mnt]),
        true,
        "the ESP has no fspec-live entry to boot while the migration runs; install the live entry first",
    );
    plan.expect(
        Cmd::new("test")
            .arg("-e")
            .arg(s(&paths.system("/sys/firmware/efi/efivars/LoaderEntryDefault-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f"))),
        false,
        "the EFI variable LoaderEntryDefault outranks loader.conf; clear it with `bootctl set-default \"\"` first",
    );
    plan.undo(Cmd::new("sh").args([
        "-c",
        r#"set -e; [ -f "$2" ] || exit 0; cp "$2" "$1.fspec-tmp"; sync "$1.fspec-tmp"; mv "$1.fspec-tmp" "$1"; sync -f "$1""#,
        "sh",
        &format!("{esp_mnt}/loader/loader.conf"),
        &format!("{state}/loader.conf"),
    ]));
    plan.run(Cmd::new("sh").args([
        "-c",
        r#"set -e; cp "$1" "$2.part"; sync "$2.part"; mv "$2.part" "$2"
{ grep -v '^default ' "$1" || true; echo 'default fspec-live*'; } > "$1.fspec-tmp"
sync "$1.fspec-tmp"; mv "$1.fspec-tmp" "$1"; sync -f "$1""#,
        "sh",
        &format!("{esp_mnt}/loader/loader.conf"),
        &format!("{state}/loader.conf"),
    ]));
    steps.push(Step {
        what: "boot the fspec-live entry by default until the migration is done".into(),
        destructive: false,
        plan,
    });

    let mut plan = Plan::default();
    plan.undo(Cmd::new("sh").args([
        "-c",
        r#"umount -R "$1" 2>/dev/null; if [ "$(blkid -o value -s TYPE "$2")" = btrfs ]; then btrfs-convert -r "$2"; fi"#,
        "sh",
        &disk,
        dev,
    ]));
    plan.run(Cmd::new("e2fsck").args(["-fy", dev]));
    plan.run(align_ext4(dev));
    plan.run(Cmd::new("btrfs-convert").args(["--uuid", "copy", dev]));
    let what = format!(
        "convert the ext4 root {dev} to btrfs in place, keeping its UUID and the ext4 image as ext2_saved for \
         `btrfs-convert -r`"
    );
    steps.push(Step { what, destructive: true, plan });

    let mut plan = Plan::default();
    mount_top(&mut plan, &top, dev.into());
    plan.expect(Cmd::new("test").arg("-e").arg(format!("{top}/@nixos")), false, "@nixos already exists");
    plan.run(Cmd::new("btrfs").args(["subvolume", "snapshot", &top]).arg(format!("{top}/@nixos")));
    for v in std::iter::once("@home").chain(SHARED_SUBVOLS) {
        plan.run(Cmd::new("btrfs").args(["subvolume", "create"]).arg(format!("{top}/{v}")));
    }
    plan.run(reflink_copy(&format!("{top}/home"), &format!("{top}/@home")));
    plan.run(reflink_copy(&format!("{top}/opt/fspec"), &format!("{top}/@fspec")));
    // The swapfile NixOS creates in @swap has to be no copy-on-write, which only an empty
    // directory passes on to new files.
    plan.run(Cmd::new("chattr").arg("+C").arg(format!("{top}/@swap")));
    plan.run(Cmd::new("mkdir").arg("-p").arg(format!("{top}/@nixos/swap")));
    // The live system's ISO and overlay, which its ESP entry finds by path from the top level, so
    // they have to outlive the old top-level files.
    plan.run(Cmd::new("sh").args([
        "-c",
        r#"if [ -d "$1/var/lib/fspec-live" ]; then cp -a --reflink=always "$1/var/lib/fspec-live" "$1/@fspec/live" && sync -f "$1/@fspec/live"; fi"#,
        "sh",
        &top,
    ]));
    steps.push(Step {
        what: "snapshot the top level into @nixos, and create @home with a reflink copy of /home, @swap without \
               copy-on-write, @snapshots and @fspec"
            .into(),
        destructive: false,
        plan,
    });

    let mut plan = Plan::default();
    let root = s(&paths.system(format!("{DISK}/nixos")));
    let esp = format!("{root}{}", r.esp_at);
    let saved = format!("{state}/esp");
    plan.run(Cmd::new("mkdir").arg("-p").arg(&root));
    plan.run(Cmd::new("mount").arg("-o").arg(format!("subvol=@nixos,{BTRFS_OPTS}")).args([dev, &root]));
    plan.cleanup.push(Cmd::new("umount").arg("-R").arg(&root));
    plan.run(Cmd::new("mkdir").arg("-p").arg(&esp));
    plan.run(Cmd::new("mount").arg("--bind").arg(esp_mnt).arg(&esp));
    plan.undo(Cmd::new("sh").args([
        "-c",
        r#"set -e; [ -d "$2" ] || exit 0; for d in $3; do rm -rf "$1/$d"; if [ -e "$2/$d" ]; then mkdir -p "$(dirname "$1/$d")"; cp -a "$2/$d" "$1/$d"; fi; done; sync -f "$1""#,
        "sh",
        esp_mnt,
        &saved,
        ESP_DIRS,
    ]));
    plan.run(Cmd::new("sh").args([
        "-c",
        r#"set -e; rm -rf "$2" "$2.part"; mkdir -p "$2.part"; for d in $3; do if [ -e "$1/$d" ]; then mkdir -p "$(dirname "$2.part/$d")"; cp -a "$1/$d" "$2.part/$d"; fi; done; sync -f "$2.part"; mv "$2.part" "$2""#,
        "sh",
        esp_mnt,
        &saved,
        ESP_DIRS,
    ]));
    for (from, to) in [("/dev", "dev"), ("/sys", "sys")] {
        plan.run(Cmd::new("mount").arg("--rbind").arg(from).arg(format!("{root}/{to}")));
        plan.run(Cmd::new("mount").arg("--make-rslave").arg(format!("{root}/{to}")));
    }
    plan.run(Cmd::new("mount").args(["-t", "proc", "proc"]).arg(format!("{root}/proc")));
    plan.run(Cmd::new("mount").args(["-t", "tmpfs", "tmpfs"]).arg(format!("{root}/run")));
    // bootctl writes the boot entry variables through efivarfs, which a live system may not mount.
    plan.run(Cmd::new("sh").args([
        "-c",
        r#"d="$1/sys/firmware/efi/efivars"; if [ -d "$d" ] && ! mountpoint -q "$d"; then mount -t efivarfs efivarfs "$d"; fi"#,
        "sh",
        &root,
    ]));
    let next = r.generation + 1;
    let sys = r.system.as_str();
    // Store binaries only run against their own store, so the chroot is @nixos and not the live
    // root, and the environment is set inside it, where the live root's PATH means nothing.
    let chroot = |args: &[&str]| {
        Cmd::new("chroot")
            .arg(&root)
            .arg(format!("{sys}/sw/bin/env"))
            .arg(format!("PATH={sys}/sw/bin"))
            .arg("HOME=/root")
            .arg(format!("LOCALE_ARCHIVE={sys}/sw/lib/locale/locale-archive"))
            .args(args.iter().copied())
    };
    let profile = "/nix/var/nix/profiles/system";
    plan.run(chroot(&[&format!("{sys}/sw/bin/nix-env"), "-p", profile, "--set", sys]));
    plan.expect(
        // The link points into @nixos's store, which `-e` would look for in the live root's.
        Cmd::new("test").arg("-L").arg(format!("{root}{profile}-{next}-link")),
        true,
        format!("nix-env did not make {sys} generation {next} of the system profile"),
    );
    // The profile keeps the system alive from here on.
    plan.run(Cmd::new("rm").arg("-f").arg(format!("{root}{STAGED_ROOT}")));
    plan.run(chroot(&[&format!("{sys}/bin/switch-to-configuration"), "boot"]));
    // Newer NixOS names its entries by hash instead of nixos-generation-<n>.conf.
    plan.expect(
        Cmd::new("sh").args(["-c", r#"grep -qs "init=$2/init" "$1"/loader/entries/*.conf"#, "sh", &esp, sys]),
        true,
        format!("switch-to-configuration wrote no boot entry for {sys}"),
    );
    // Only the new entry can boot the btrfs root, so it gets three tries, its initrd panics and
    // reboots on failure instead of waiting in emergency mode, the booted system goes on to sshd
    // past a failed mount, since `systemd.mask=` only applies after the initrd, and the ext4
    // generations move out of systemd-boot's sight. Without a `default`, systemd-boot boots the
    // first entry in its order, which puts entries out of tries last, so the fspec-live entry
    // follows three failures. Once a boot is blessed the entry loses the suffix again, through
    // `systemd.run=` since the NixOS root is not ours to add units to, so later boots get the
    // emergency shells back and the entry's content matches the hash NixOS named it by.
    let unmask = format!("{}/loader/fspec-unmask", r.esp_at.trim_end_matches('/'));
    let trial = format!(
        "{TRIAL} systemd.run=\"/bin/sh {unmask}\" systemd.run_success_action=none systemd.run_failure_action=none \
         {RUN_BESIDE}"
    );
    plan.run(Cmd::new("sh").args([
        "-c",
        r#"set -e; e="$1/loader/entries"; new=$(grep -ls "init=$2/init" "$e"/*.conf | head -n 1)
base=${new##*/}; base=${base%.conf}; base=${base%%+*}
printf '%s\n' "$4" > "$1/loader/fspec-unmask"
sed -i "s|^options .*|& $3|" "$new"
mv "$new" "$e/$base+3.conf"
mkdir -p "$1/loader/entries.ext4"
for f in "$e"/nixos-*.conf; do if [ "${f##*/}" != "$base+3.conf" ]; then mv "$f" "$1/loader/entries.ext4/"; fi; done
sed -i '/^default /d' "$1/loader/loader.conf"
sync -f "$1/loader""#,
        "sh",
        &esp,
        sys,
        &trial,
        &unmask_script(&trial),
    ]));
    // The next nixos-rebuild would write entries for the ext4 generations again. The ext4 image
    // in ext2_saved keeps them for `btrfs-convert -r`, and their entries stay in entries.ext4.
    plan.run(chroot(&[&format!("{sys}/sw/bin/nix-env"), "-p", profile, "--delete-generations", "old"]));
    mount_top(&mut plan, &top, dev.into());
    plan.run(Cmd::new("sh").args([
        "-c",
        r#"set -e; found=; for f in "$1"/loader/entries/fspec-live*.conf; do
[ -e "$f" ] || continue; found=1
for p in $(grep -oE '(findiso|fspec\.overlay)=/var/lib/fspec-live/[^ ]+' "$f" | sed 's#.*/##'); do
  [ -f "$2/@fspec/live/$p" ] || { echo "$p is not in @fspec/live" >&2; exit 1; }
done
sed -E 's#((findiso|fspec\.overlay)=)/var/lib/fspec-live/#\1/@fspec/live/#g' "$f" |
  while IFS= read -r l; do case $l in "options "*"$3") l=${l%"$3"};; esac; printf '%s\n' "$l"; done > "$f.fspec-tmp"
sync "$f.fspec-tmp"; mv "$f.fspec-tmp" "$f"; sync -f "$f"
done
[ -n "$found" ] || echo "no fspec-live entry on the ESP, so no live ISO to point at @fspec/live""#,
        "sh",
        &esp,
        &top,
        &format!(" {LIVE_MIGRATE}"),
    ]));
    let what = format!(
        "make {sys} generation {next} of NixOS in @nixos and its boot default with three tries, move generation {} \
         and older to loader/entries.ext4 on the ESP, and point the fspec-live entry at @fspec/live without the \
         staged migration",
        r.generation
    );
    steps.push(Step { what, destructive: false, plan });
    private(paths, steps)
}

/// A transient unit that recompresses `@home` and `@nixos` on `device` with zstd, which
/// btrfs-convert leaves uncompressed. It skips the store, whose files are hardlinked, and the old
/// swapfile, which is deleted later. Rewriting a file unshares it from the old top level and
/// ext2_saved until those are deleted, so it stops once the filesystem is 80% full.
pub fn compress(paths: &Paths, device: &str) -> Cmd {
    let top = s(&paths.system(format!("{DISK}-compress")));
    let script = r#"mkdir -p "$1" && mount -o "subvolid=5,$3" "$2" "$1" || exit 1
find "$1/@home" "$1/@nixos" -xdev \( -path "$1/@nixos/nix/store" -o -path "$1/@nixos/swapfile" \) -prune -o -type f -print0 |
  xargs -0 -r -n 256 sh -c 'used=$(df --output=pcent "$0" | tail -n 1 | tr -dc 0-9); [ "$used" -lt 80 ] || exit 255; btrfs filesystem defragment -czstd "$@" || true' "$1"
umount "$1""#;
    Cmd::new("systemd-run")
        .args(["--unit=fspec-compress", "--collect", "sh", "-c", script, "sh"])
        .args([&top, device, BTRFS_OPTS])
}

/// Where `disk migrate --stage` puts the live ISO and its overlay on the running root.
/// [`nixos_migrate`] moves them to `@fspec/live`, so the paths here are what it rewrites.
pub const LIVE_DIR: &str = "/var/lib/fspec-live";
const LIVE_ISO: &str = "formalspec-installer-amd64.iso";

/// The GC root that keeps the staged NixOS system in the store until the migration has made it a
/// generation of the system profile.
const STAGED_ROOT: &str = "/nix/var/nix/gcroots/fspec-migrate";

/// The end of the staged live entry's options that runs the migration; [`nixos_migrate`] drops
/// it, so later boots of the entry are a plain live system. The script comes with the overlay,
/// which nothing orders the command line's unit after.
pub(crate) const LIVE_MIGRATE: &str = "fspec.migrate systemd.run=\"/bin/sh -c 'systemctl start \
                            fspec-live-overlay.service; exec /bin/sh /etc/fspec-live/migrate'\" \
                            systemd.run_success_action=none systemd.run_failure_action=none \
                            systemd.wants=kernel-command-line.service";

/// What `disk migrate --stage` needs to boot the live ISO once and migrate the running NixOS
/// root `root` from it.
pub struct Stage<'a> {
    pub root: &'a NixosRoot,
    /// The root filesystem's UUID, which `btrfs-convert --uuid copy` keeps.
    pub uuid: &'a str,
    pub iso: &'a str,
    /// A per-host tarball for the live system (network, ssh keys, `guard.conf`), uncompressed so
    /// fspec can append to it.
    pub overlay: Option<&'a str>,
    /// A static fspec, which the live system runs instead of its own.
    pub fspec: &'a str,
    /// Where the running system mounts the ESP, and the ESP's filesystem UUID.
    pub esp: &'a str,
    pub esp_uuid: &'a str,
    pub hostname: &'a str,
    /// Options of the running kernel to keep, such as `console=`.
    pub keep: &'a [String],
}

/// The guard the live system runs when the overlay brings none: wait ten minutes for sshd, arm
/// the hardware watchdog, and bless the boot on the ESP.
fn guard_conf(esp_uuid: &str) -> String {
    format!("TIMEOUT=600\nWATCHDOG_SEC=30\nESP=UUID={esp_uuid}\n")
}

/// The script the staged entry starts through `systemd.run=`. It runs the migration once the
/// guard has the machine reachable, keeps the log on the ESP, and reboots unless an unfinished
/// journal is left, which needs someone on ssh. A boot without `fspec.migrate` and without a
/// journal is a plain live system.
fn migrate_script(s: &Stage) -> String {
    let (dev, sys, esp_uuid) = (s.root.device.as_str(), s.root.system.as_str(), s.esp_uuid);
    format!(
        r#"#!/bin/sh
journal=/run/fspec/esp/fspec/disk-journal/journal.json
m=$(mktemp -d); j=
if mount -o ro UUID={esp_uuid} "$m"; then [ -e "$m/fspec/disk-journal/journal.json" ] && j=1; umount "$m"; fi
rmdir "$m"
grep -qw fspec.migrate /proc/cmdline || [ -n "$j" ] || exit 0
if [ -f /etc/fspec-live/guard.conf ]; then
	until systemctl -q is-active fspec-live-guard.service; do
		systemctl -q is-failed fspec-live-guard.service && break
		sleep 5
	done
fi
dev=$(readlink -f /dev/disk/by-uuid/{uuid})
[ -b "$dev" ] || dev={dev}
log=/run/fspec-migrate.log
echo "fspec-live: migrating $dev" > /dev/kmsg
printf '%s\n' "$dev" | /usr/local/bin/fspec disk migrate "$dev" --nixos-system {sys} --no-backup > "$log" 2>&1
echo "exit $?" >> "$log"
mkdir -p /run/fspec/esp
mountpoint -q /run/fspec/esp || mount UUID={esp_uuid} /run/fspec/esp
mkdir -p /run/fspec/esp/fspec
cp "$log" /run/fspec/esp/fspec/migrate.log
sync -f /run/fspec/esp/fspec
if [ -e "$journal" ]; then
	echo "fspec-live: the migration left $journal; staying up" > /dev/kmsg
	exit 1
fi
systemctl reboot
"#,
        uuid = s.uuid,
    )
}

/// The steps that, on the running NixOS root on ext4, stage the live ISO and a one-shot
/// `fspec-live` entry that migrates this root on the next boot: the ISO and an overlay with a
/// static fspec and the migration script go to [`LIVE_DIR`], the live kernel and initrd to the
/// ESP. The entry boots toram, so the root is unmounted while it converts, and has three boot
/// tries, so a live boot that fails or never comes online falls back to the default entry.
pub fn stage_live(paths: &Paths, s: &Stage) -> Plan {
    let live = s_path(paths, LIVE_DIR);
    let iso = format!("{live}/{LIVE_ISO}");
    let overlay = format!("{live}/overlay.tar");
    let work = s_path(paths, "/run/fspec/stage");
    let files = format!("{work}/files");
    let mnt = format!("{work}/iso");
    let esp = s_path(paths, s.esp);
    let mut plan = Plan::default();
    plan.expect(
        Cmd::new("test")
            .arg("-e")
            .arg(s_path(paths, "/sys/firmware/efi/efivars/LoaderEntryDefault-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f")),
        false,
        "the EFI variable LoaderEntryDefault outranks loader.conf; clear it with `bootctl set-default \"\"` first",
    );
    // The migration finds the ISO by its path from the root of the ext4 filesystem.
    plan.expect(
        Cmd::new("sh").args(["-c", r#"[ "$(findmnt -nvo SOURCE -T "$1")" = "$2" ]"#, "sh", "/var/lib", &s.root.device]),
        true,
        format!("/var/lib is not on {}, so the live system would not find the ISO there", s.root.device),
    );
    plan.run(Cmd::new("install").args(["-d", "-m", "0700", &live]));
    plan.run(Cmd::new("install").args(["-m", "0600", s.iso, &iso]));
    plan.run(Cmd::new("mkdir").arg("-p").arg(format!("{files}/usr/local/bin")).arg(format!("{files}/etc/fspec-live")));
    plan.cleanup.push(Cmd::new("rm").args(["-rf", &work]));
    plan.run(Cmd::new("install").args(["-m", "0755", s.fspec, &format!("{files}/usr/local/bin/fspec")]));
    plan.run(
        Cmd::new("sh")
            .args(["-c", r#"cat > "$1" && chmod 0700 "$1""#, "sh", &format!("{files}/etc/fspec-live/migrate")])
            .stdin(migrate_script(s)),
    );
    let tar = |c: Cmd| c.args(["--owner=0", "--group=0", "-C", &files, "usr", "etc"]);
    match s.overlay {
        Some(o) => {
            // A guard.conf of the host's own wins; tar extracts a later member over an earlier one.
            plan.run(
                Cmd::new("sh")
                    .args([
                        "-c",
                        r#"tar -tf "$1" | grep -qE '^(\./)?etc/fspec-live/guard\.conf$' || cat > "$2""#,
                        "sh",
                        o,
                        &format!("{files}/etc/fspec-live/guard.conf"),
                    ])
                    .stdin(guard_conf(s.esp_uuid)),
            );
            plan.run(Cmd::new("install").args(["-m", "0600", o, &overlay]));
            plan.run(tar(Cmd::new("tar").args(["-rf", &overlay])));
        }
        None => {
            plan.run(
                Cmd::new("sh")
                    .args(["-c", r#"cat > "$1""#, "sh", &format!("{files}/etc/fspec-live/guard.conf")])
                    .stdin(guard_conf(s.esp_uuid)),
            );
            plan.run(tar(Cmd::new("tar").args(["-cf", &overlay])));
            plan.run(Cmd::new("chmod").args(["0600", &overlay]));
        }
    }
    plan.run(Cmd::new("mkdir").arg("-p").arg(&mnt));
    plan.run(Cmd::new("mount").args(["-o", "loop,ro", &iso, &mnt]));
    plan.cleanup.insert(0, Cmd::new("umount").arg(&mnt));
    plan.run(Cmd::new("install").args(["-d", &format!("{esp}/fspec-live")]));
    for f in ["vmlinuz", "initrd.img"] {
        plan.run(Cmd::new("install").args([
            "-m",
            "0644",
            &format!("{mnt}/live/{f}"),
            &format!("{esp}/fspec-live/{f}"),
        ]));
    }
    let mut options = vec![
        "boot=live".to_string(),
        "components".into(),
        format!("findiso={LIVE_DIR}/{LIVE_ISO}"),
        format!("fspec.overlay={LIVE_DIR}/overlay.tar"),
        "username=root".into(),
        format!("hostname={}-live", s.hostname),
        "locales=en_US.UTF-8".into(),
        "keyboard-layouts=us".into(),
        // live-boot copies the ISO into RAM and unmounts the root, which is then free to convert.
        "toram".into(),
        // live-boot drops to a shell when it cannot find the ISO; this reboots out of it instead.
        "panic=20".into(),
    ];
    options.extend(s.keep.iter().cloned());
    options.push(LIVE_MIGRATE.into());
    let entry = format!(
        "title   FormalSpec live\nlinux   /fspec-live/vmlinuz\ninitrd  /fspec-live/initrd.img\noptions {}\n",
        options.join(" ")
    );
    plan.run(
        Cmd::new("sh")
            .args([
                "-c",
                r#"set -e; e="$1/loader/entries"; rm -f "$e"/fspec-live*.conf; cat > "$e/fspec-live.fspec-tmp"
sync "$e/fspec-live.fspec-tmp"; mv "$e/fspec-live.fspec-tmp" "$e/fspec-live+3.conf"; sync -f "$e""#,
                "sh",
                &esp,
            ])
            .stdin(entry),
    );
    plan.run(Cmd::new("ln").args(["-sfn", &s.root.system, &s_path(paths, STAGED_ROOT)]));
    plan.run(Cmd::new("sync"));
    plan.run(Cmd::new("bootctl").args(["set-oneshot", "fspec-live.conf"]));
    plan
}

fn s_path(paths: &Paths, p: &str) -> String {
    s(&paths.system(p))
}
