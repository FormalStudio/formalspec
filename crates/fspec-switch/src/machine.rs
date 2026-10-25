use anyhow::{Context, Result, bail};
use fspec_core::Paths;
use fspec_core::facts::os_release_id;
use fspec_core::runner::{Cmd, Runner};
use serde_json::Value;

pub const ESP_TYPE: &str = "c12a7328-f81f-11d2-ba4b-00a0c93ec93b";
pub const BIOS_GRUB_TYPE: &str = "21686148-6449-6e6f-744e-656564454649";
/// The Extended Boot Loader partition systemd-boot reads entries and kernels from next to its ESP.
pub const XBOOTLDR_TYPE: &str = "bc13c2ff-59e6-4262-a352-b275fd6f7172";
/// Where every root mounts Windows' ESP when `/efi` is the XBOOTLDR partition beside it.
pub const LOADER_ESP: &str = "/boot/efi";
/// Where the BIOS layout mounts its shared boot partition in every root.
pub const SHARED_BOOT: &str = "/var/lib/fspec/boot";
/// The subvolumes every root of the default and BIOS layouts shares, besides the home.
pub const SHARED_SUBVOLS: [&str; 3] = ["@swap", "@snapshots", "@fspec"];
/// The options fspec mounts the btrfs filesystem with.
pub const BTRFS_OPTS: &str = "noatime,compress=zstd:1";
pub const GIB: u64 = 1 << 30;
pub const MIB: u64 = 1 << 20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Firmware {
    Efi,
    Bios,
}

/// A partition, as lsblk reports it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Part {
    pub dev: String,
    pub uuid: String,
    pub partuuid: String,
    pub parttype: String,
    /// The GPT partition name.
    pub label: String,
    pub fstype: String,
    pub size: u64,
    /// Free bytes, when it is mounted.
    pub avail: Option<u64>,
    pub partn: u64,
    /// First sector.
    pub start: u64,
    pub mounts: Vec<String>,
}

/// The btrfs filesystem holding the roots.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Btrfs {
    pub uuid: String,
    /// UUID of the LUKS2 container it sits in.
    pub luks: Option<String>,
    /// The subvolume mounted at `/home`, as findmnt prints it.
    pub home: String,
}

/// The disk layouts `switch --distro` installs roots side by side on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Layout {
    /// GPT, the ESP at `/efi` with systemd-boot, and btrfs (optionally in LUKS2) with a
    /// subvolume per root plus the shared ones. Beside Windows, `esp` is an XBOOTLDR partition
    /// holding the kernels and entries, and `loader` is Windows' own ESP at [`LOADER_ESP`], too
    /// small for kernels, holding systemd-boot and `loader.conf`.
    Default { esp: Part, btrfs: Btrfs, loader: Option<Box<Part>> },
    /// A pool with `<pool>/ROOT/<distro>` boot environments, booted by ZFSBootMenu from the ESP.
    Zfs { esp: Part, pool: String },
    /// GRUB in BIOS mode with a shared ext4 boot partition at [`SHARED_BOOT`], and btrfs.
    Bios { boot: Part, btrfs: Btrfs },
    /// Anything else, with what keeps it from being one of the above.
    Unsupported { reasons: Vec<String> },
}

impl Layout {
    pub fn name(&self) -> &'static str {
        match self {
            Layout::Default { .. } => "default",
            Layout::Zfs { .. } => "zfs",
            Layout::Bios { .. } => "bios",
            Layout::Unsupported { .. } => "unsupported",
        }
    }

    /// Where systemd-boot's `loader.conf` and binaries live in every root: `/efi`, or Windows' ESP.
    pub fn loader_dir(&self) -> &'static str {
        match self {
            Layout::Default { loader: Some(_), .. } => LOADER_ESP,
            _ => "/efi",
        }
    }

    pub fn btrfs(&self) -> Option<&Btrfs> {
        match self {
            Layout::Default { btrfs, .. } | Layout::Bios { btrfs, .. } => Some(btrfs),
            _ => None,
        }
    }
}

/// What the plans need to know about the running system.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Machine {
    pub layout: Layout,
    /// The booted root: a btrfs subvolume as findmnt prints it (`/@arch`), or a ZFS dataset
    /// (`rpool/ROOT/arch`).
    pub root: String,
    /// `ID=` of the running `/etc/os-release`.
    pub running: String,
    pub firmware: Firmware,
    /// The whole disk holding the root, where BIOS GRUB goes.
    pub disk: String,
    /// The running kernel's command line. Entries fspec writes keep its options, like `console=`
    /// and `net.ifnames=0`, which the machine's network and serial setup can depend on.
    pub cmdline: String,
    /// Partitions of `disk`.
    pub parts: Vec<Part>,
    /// The partition holding the root filesystem, or its LUKS container.
    pub root_part: Part,
    /// Type and UUID of the root filesystem.
    pub fstype: String,
    pub fs_uuid: String,
    pub luks: Option<String>,
    /// What detection read, for `fspec disk plan`.
    pub facts: Facts,
}

/// A tool that is not installed answers no, like one that fails.
pub(crate) fn probe(runner: &dyn Runner, cmd: Cmd) -> Result<Option<String>> {
    let out = match runner.run(&cmd) {
        Ok(out) => out,
        Err(e) if crate::not_installed(&e) => return Ok(None),
        Err(e) => return Err(e),
    };
    Ok((out.status == 0).then(|| out.stdout.trim().to_string()))
}

/// What detection reads, kept apart so classification is a pure function.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    /// `findmnt -no FSTYPE,SOURCE,FSROOT -v -T /`.
    pub root_mount: String,
    /// [`LSBLK`]'s output.
    pub lsblk: String,
    pub efi: bool,
    /// FSROOT of `/home`, `/opt/fspec` and `/boot`, empty when they are not mounted.
    pub home: String,
    pub fspec: String,
    pub boot: String,
    /// Top-level btrfs subvolumes, or the pool's datasets.
    pub volumes: Vec<String>,
    /// `zpool list -vHP <pool>` for a ZFS root.
    pub zpool: String,
}

pub const LSBLK: [&str; 6] = [
    "lsblk",
    "-J",
    "-b",
    "-p",
    "-o",
    "NAME,TYPE,FSTYPE,SIZE,UUID,PARTUUID,PARTTYPE,PARTLABEL,PTTYPE,MOUNTPOINTS,FSAVAIL,PARTN,START",
];

/// Read the running system through `runner`.
pub fn detect(runner: &dyn Runner, paths: &Paths) -> Result<Machine> {
    let sys = |p: &str| paths.system(p).display().to_string();
    // Without -T, findmnt only answers for a mount point.
    let fsroot = |p: &str| -> Result<String> {
        Ok(probe(runner, Cmd::new("findmnt").args(["-no", "FSROOT"]).arg(sys(p)))?.unwrap_or_default())
    };
    let Some(root_mount) =
        probe(runner, Cmd::new("findmnt").args(["-no", "FSTYPE,SOURCE,FSROOT", "-v", "-T"]).arg(sys("/")))?
    else {
        bail!("could not read the root filesystem with findmnt");
    };
    let lsblk = probe(runner, Cmd::new(LSBLK[0]).args(LSBLK[1..].iter().copied()))?
        .context("lsblk could not list the disks")?;
    let mut facts = Facts {
        efi: probe(runner, Cmd::new("test").arg("-d").arg(sys("/sys/firmware/efi")))?.is_some(),
        home: fsroot("/home")?,
        fspec: fsroot("/opt/fspec")?,
        boot: fsroot("/boot")?,
        root_mount,
        lsblk,
        ..Facts::default()
    };
    match facts.root_mount.split_whitespace().next() {
        Some("btrfs") => {
            let list = probe(runner, Cmd::new("btrfs").args(["subvolume", "list"]).arg(sys("/")))?.unwrap_or_default();
            facts.volumes = top_level_subvols(&list);
        }
        Some("zfs") => {
            let dataset = facts.root_mount.split_whitespace().nth(1).unwrap_or_default();
            let pool = dataset.split('/').next().unwrap_or_default().to_string();
            let list =
                probe(runner, Cmd::new("zfs").args(["list", "-H", "-o", "name", "-r", &pool]))?.unwrap_or_default();
            facts.volumes = list.lines().map(String::from).collect();
            facts.zpool = probe(runner, Cmd::new("zpool").args(["list", "-vHP", &pool]))?.unwrap_or_default();
        }
        _ => {}
    }
    let running = std::fs::read_to_string(paths.system("/etc/os-release"))
        .ok()
        .and_then(|s| os_release_id(&s))
        .unwrap_or_default();
    let cmdline = std::fs::read_to_string(paths.system("/proc/cmdline")).unwrap_or_default().trim().to_string();
    classify(&facts, running, cmdline)
}

/// Read the disk of the btrfs filesystem on `device` from a system that does not run from it, such
/// as a live system, taking the subvolume `root` (`@nixos`) as its root.
pub fn detect_device(runner: &dyn Runner, paths: &Paths, device: &str, root: &str) -> Result<Machine> {
    // lsblk names devices by their /dev path, not by a /dev/disk/by-* link.
    let device = std::fs::canonicalize(device).with_context(|| format!("{device} does not exist"))?;
    let lsblk = probe(runner, Cmd::new(LSBLK[0]).args(LSBLK[1..].iter().copied()))?
        .context("lsblk could not list the disks")?;
    let facts = Facts {
        efi: probe(runner, Cmd::new("test").arg("-d").arg(paths.system("/sys/firmware/efi").display().to_string()))?
            .is_some(),
        root_mount: format!("btrfs {} /{}", device.display(), root.trim_start_matches('/')),
        lsblk,
        ..Facts::default()
    };
    let cmdline = std::fs::read_to_string(paths.system("/proc/cmdline")).unwrap_or_default().trim().to_string();
    classify(&facts, String::new(), cmdline)
}

/// Names of the subvolumes directly under the btrfs top level, from `btrfs subvolume list`.
pub(crate) fn top_level_subvols(list: &str) -> Vec<String> {
    list.lines()
        .filter(|l| l.contains(" top level 5 "))
        .filter_map(|l| l.split(" path ").nth(1))
        .map(|p| p.trim().to_string())
        .collect()
}

/// A block device from lsblk's tree, with the index of its parent.
struct Dev<'a> {
    v: &'a Value,
    parent: Option<usize>,
}

fn flatten<'a>(v: &'a Value, parent: Option<usize>, out: &mut Vec<Dev<'a>>) {
    out.push(Dev { v, parent });
    let me = out.len() - 1;
    for c in v["children"].as_array().into_iter().flatten() {
        flatten(c, Some(me), out);
    }
}

fn text(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or_default().to_string()
}

pub(crate) fn part(v: &Value) -> Part {
    Part {
        dev: text(v, "name"),
        uuid: text(v, "uuid"),
        partuuid: text(v, "partuuid"),
        parttype: text(v, "parttype").to_lowercase(),
        label: text(v, "partlabel"),
        fstype: text(v, "fstype"),
        size: v["size"].as_u64().unwrap_or_default(),
        avail: v["fsavail"].as_u64(),
        partn: v["partn"].as_u64().unwrap_or_default(),
        start: v["start"].as_u64().unwrap_or_default(),
        mounts: v["mountpoints"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m.as_str())
            .map(String::from)
            .collect(),
    }
}

fn mounted_at(v: &Value, target: &str) -> bool {
    v["mountpoints"].as_array().into_iter().flatten().any(|m| m.as_str() == Some(target))
}

/// Sort the running system into a [`Layout`].
pub fn classify(f: &Facts, running: String, cmdline: String) -> Result<Machine> {
    let json: Value = serde_json::from_str(&f.lsblk).context("parsing lsblk's JSON")?;
    let mut devs = Vec::new();
    for d in json["blockdevices"].as_array().into_iter().flatten() {
        flatten(d, None, &mut devs);
    }
    let disk_of = |mut i: usize| {
        while let Some(p) = devs[i].parent {
            i = p;
        }
        i
    };
    let find = |name: &str| devs.iter().position(|d| text(d.v, "name") == name);

    let mut fields = f.root_mount.split_whitespace();
    let (fstype, source) = (fields.next().unwrap_or_default(), fields.next().unwrap_or_default());
    let fsroot = fields.next().unwrap_or("/");
    let firmware = if f.efi { Firmware::Efi } else { Firmware::Bios };

    // The device the root filesystem sits on: a partition, a LUKS mapping, or a pool member.
    let root_dev = match fstype {
        "zfs" => {
            f.zpool.lines().filter_map(|l| l.split_whitespace().next()).find(|d| d.starts_with("/dev/")).and_then(find)
        }
        _ => find(source),
    };
    let Some(root_dev) = root_dev else { bail!("lsblk does not list {source}, the device holding /") };
    let disk_i = disk_of(root_dev);
    let disk = text(devs[disk_i].v, "name");
    let on_disk: Vec<&Dev> = devs.iter().enumerate().filter(|(i, _)| disk_of(*i) == disk_i).map(|(_, d)| d).collect();
    let gpt = text(devs[disk_i].v, "pttype") == "gpt";
    let esp_at = |target: &str| {
        on_disk.iter().find(|d| text(d.v, "parttype").to_lowercase() == ESP_TYPE && mounted_at(d.v, target))
    };
    // Beside Windows, /efi is an XBOOTLDR partition and Windows' ESP is at LOADER_ESP.
    let xbootldr = on_disk
        .iter()
        .find(|d| text(d.v, "parttype").to_lowercase() == XBOOTLDR_TYPE && mounted_at(d.v, "/efi"))
        .map(|d| part(d.v));
    let loader = xbootldr.as_ref().and_then(|_| esp_at(LOADER_ESP)).map(|d| Box::new(part(d.v)));
    let esp = if loader.is_some() { xbootldr } else { esp_at("/efi").map(|d| part(d.v)) };
    let no_esp = || match ["/boot/efi", "/boot"].into_iter().find(|t| esp_at(t).is_some()) {
        Some(t) => format!("the ESP is mounted at {t}, not /efi"),
        None => "no ESP is mounted at /efi".into(),
    };

    let parts: Vec<Part> = on_disk.iter().filter(|d| text(d.v, "type") == "part").map(|d| part(d.v)).collect();
    let crypt = text(devs[root_dev].v, "type") == "crypt";
    let luks = devs[root_dev].parent.filter(|_| crypt).map(|p| text(devs[p].v, "uuid"));
    let root_part = match devs[root_dev].parent {
        Some(p) if crypt => part(devs[p].v),
        _ => part(devs[root_dev].v),
    };
    let fs_uuid = text(devs[root_dev].v, "uuid");

    let mut reasons = Vec::new();
    let root = if fstype == "zfs" { source.to_string() } else { fsroot.to_string() };
    let layout = match fstype {
        "btrfs" => {
            let own = format!("/@{running}");
            if root == "/" {
                reasons.push(format!("the root sits at the btrfs top level instead of in @{running}"));
            } else if root != own {
                reasons.push(format!("the root is the subvolume {root}, not {own}"));
            }
            if f.home.is_empty() || f.home == root {
                reasons.push("/home is not its own subvolume".into());
            }
            let missing: Vec<&str> =
                SHARED_SUBVOLS.iter().copied().filter(|s| !f.volumes.iter().any(|v| v == s)).collect();
            if !missing.is_empty() {
                reasons.push(format!("the shared subvolumes {} are missing", missing.join(", ")));
            } else if f.fspec != "/@fspec" {
                reasons.push("/opt/fspec is not mounted from @fspec".into());
            }
            let btrfs = Btrfs { uuid: fs_uuid.clone(), luks: luks.clone(), home: f.home.clone() };
            match firmware {
                Firmware::Efi => {
                    if !gpt {
                        reasons.push("the disk has no GPT partition table".into());
                    }
                    if esp.is_none() {
                        reasons.push(no_esp());
                    }
                    match esp {
                        Some(esp) if reasons.is_empty() => Layout::Default { esp, btrfs, loader },
                        _ => Layout::Unsupported { reasons },
                    }
                }
                Firmware::Bios => {
                    let boot = on_disk.iter().find(|d| text(d.v, "fstype") == "ext4" && mounted_at(d.v, SHARED_BOOT));
                    if boot.is_none() {
                        reasons.push(format!("no shared ext4 boot partition is mounted at {SHARED_BOOT}"));
                    } else if f.boot != format!("/{running}") {
                        reasons.push(format!("/boot is not the folder {running}/ of the shared boot partition"));
                    }
                    if gpt && !on_disk.iter().any(|d| text(d.v, "parttype").to_lowercase() == BIOS_GRUB_TYPE) {
                        reasons.push("the GPT disk has no bios_grub partition for GRUB".into());
                    }
                    match boot {
                        Some(b) if reasons.is_empty() => Layout::Bios { boot: part(b.v), btrfs },
                        _ => Layout::Unsupported { reasons },
                    }
                }
            }
        }
        "zfs" => {
            let pool = source.split('/').next().unwrap_or_default().to_string();
            if source != format!("{pool}/ROOT/{running}") {
                reasons.push(format!("the root is the dataset {source}, not {pool}/ROOT/{running}"));
            }
            if firmware == Firmware::Bios {
                reasons.push("ZFSBootMenu boots this layout through EFI, and this machine booted through BIOS".into());
            }
            for want in ["home", "fspec"] {
                if !f.volumes.iter().any(|v| *v == format!("{pool}/{want}")) {
                    reasons.push(format!("the dataset {pool}/{want} is missing"));
                }
            }
            if esp.is_none() {
                reasons.push(no_esp());
            }
            match esp {
                Some(esp) if reasons.is_empty() => Layout::Zfs { esp, pool },
                _ => Layout::Unsupported { reasons },
            }
        }
        other => Layout::Unsupported {
            reasons: vec![format!("the root filesystem is {other}; the layouts put the roots on btrfs or ZFS")],
        },
    };
    Ok(Machine {
        layout,
        root,
        running,
        firmware,
        disk,
        cmdline,
        parts,
        root_part,
        fstype: fstype.into(),
        fs_uuid,
        luks,
        facts: f.clone(),
    })
}

/// What a `--root` run assumes, since its commands are only logged and answer nothing: the
/// default layout with the rooted system in `@<running>`.
pub fn assumed(running: &str) -> Machine {
    let esp = Part {
        dev: "/dev/sda1".into(),
        uuid: "0000-0000".into(),
        parttype: ESP_TYPE.into(),
        fstype: "vfat".into(),
        size: 2 * GIB,
        mounts: vec!["/efi".into()],
        ..Part::default()
    };
    let btrfs = Btrfs { uuid: "00000000-0000-0000-0000-000000000000".into(), luks: None, home: "/@home".into() };
    Machine {
        parts: vec![esp.clone()],
        layout: Layout::Default { esp, btrfs, loader: None },
        root: format!("/@{running}"),
        running: running.into(),
        firmware: Firmware::Efi,
        disk: "/dev/sda".into(),
        cmdline: String::new(),
        root_part: Part::default(),
        fstype: "btrfs".into(),
        fs_uuid: "00000000-0000-0000-0000-000000000000".into(),
        luks: None,
        facts: Facts::default(),
    }
}

/// The distro whose root boots by default, read from the bootloader: systemd-boot's `default`,
/// the pool's `bootfs`, or the default entry of the shared GRUB menu. `None` when none is set.
pub fn boot_owner(runner: &dyn Runner, paths: &Paths, m: &Machine) -> Result<Option<String>> {
    let read = |p: &str| std::fs::read_to_string(paths.system(p)).unwrap_or_default();
    Ok(match &m.layout {
        // fspec writes `default <distro>-*`; AerynOS's blsforme writes `default "aerynos*"`.
        Layout::Default { .. } => read(&format!("{}/loader/loader.conf", m.layout.loader_dir()))
            .lines()
            .find_map(|l| l.trim().strip_prefix("default"))
            .map(|d| d.trim().trim_matches('"'))
            .and_then(|d| d.strip_suffix("-*").or_else(|| d.strip_suffix('*')))
            .filter(|d| !d.is_empty())
            .map(String::from),
        Layout::Zfs { pool, .. } => {
            probe(runner, Cmd::new("zpool").args(["get", "-H", "-o", "value", "bootfs", pool]))?
                .and_then(|v| v.strip_prefix(&format!("{pool}/ROOT/")).map(String::from))
        }
        Layout::Bios { .. } => read(&format!("{SHARED_BOOT}/grub/grub.cfg"))
            .lines()
            .find_map(|l| l.trim().strip_prefix("set default=\"fspec-"))
            .and_then(|d| d.strip_suffix('"'))
            .map(String::from),
        Layout::Unsupported { .. } => None,
    })
}
