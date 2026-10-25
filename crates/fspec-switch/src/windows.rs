//! Installing beside Windows: reading a disk that holds Windows, deciding where Linux goes (free
//! space, or space shrunk off the Windows partition), and why Windows cannot be shrunk.

use anyhow::{Context, Result, bail};
use fspec_core::runner::Cmd;
use serde_json::Value;

use crate::Plan;
use crate::machine::{ESP_TYPE, GIB, MIB, Part};

/// Microsoft basic data, the type of Windows' `C:`.
pub const BASIC_DATA_TYPE: &str = "ebd0a0a2-b9e5-4433-87c0-68b6b72699c7";
/// Sizes the installer shows are decimal, as disk vendors and Windows' Disk Management count them.
pub const GB: u64 = 1_000_000_000;
/// The smallest space Linux takes beside Windows: the 2 GiB XBOOTLDR partition and a root of 30 GB.
pub const MIN_LINUX: u64 = 32 * GB;
/// What Windows keeps beyond the size ntfsresize could shrink it to, for updates and its pagefile.
pub const WINDOWS_HEADROOM: u64 = 20 * GIB;
/// The XBOOTLDR partition holding every root's kernels and entries.
pub const XBOOTLDR_SIZE: u64 = 2 * GIB;

/// Unallocated sectors `[start, end)`, aligned to 1 MiB.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gap {
    pub start: u64,
    pub end: u64,
}

/// A disk with Windows on it, as sfdisk and lsblk describe it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowsDisk {
    pub disk: String,
    /// Logical sector size in bytes.
    pub sector: u64,
    /// Windows' ESP, with its FAT serial as `uuid`.
    pub esp: Part,
    /// `C:`, the largest basic data partition holding NTFS or BitLocker. `start` is in sectors.
    pub windows: Part,
    /// The partition after `C:` starts here, or the disk's last usable sector + 1.
    pub windows_limit: u64,
    pub gaps: Vec<Gap>,
    pub next_partn: u64,
    /// The size ntfsresize could shrink `C:` to, or why it cannot be shrunk, in one line.
    pub shrink_min: Result<u64, String>,
}

impl WindowsDisk {
    fn sectors(&self, bytes: u64) -> u64 {
        bytes / self.sector
    }

    fn align(&self) -> u64 {
        (MIB / self.sector).max(1)
    }

    pub fn bitlocker(&self) -> bool {
        self.windows.fstype.eq_ignore_ascii_case("bitlocker")
    }

    /// The largest free space, in bytes.
    pub fn free(&self) -> u64 {
        self.gaps.iter().map(|g| (g.end - g.start) * self.sector).max().unwrap_or_default()
    }

    /// The most Linux can take from `C:`, or why it can take nothing.
    pub fn max_linux(&self) -> Result<u64, String> {
        if self.bitlocker() {
            return Err(bitlocker_refusal());
        }
        let min = self.shrink_min.clone()?;
        let keep = min + WINDOWS_HEADROOM;
        let most = self.windows.size.saturating_sub(keep) / GB * GB;
        if most < MIN_LINUX {
            return Err(format!(
                "Windows has too little free space to give {} GB. Free some up in Windows first.",
                MIN_LINUX / GB
            ));
        }
        Ok(most)
    }
}

/// What `disk init` needs to install beside Windows instead of erasing the disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BesideWindows {
    pub disk: WindowsDisk,
    pub space: Space,
}

/// Shrink `C:` when the space asks for it, then create the XBOOTLDR and root partitions in the
/// space. Returns their partition numbers.
pub(crate) fn partitions(plan: &mut Plan, b: &BesideWindows, root_code: &str) -> Result<(u64, u64)> {
    let w = &b.disk;
    let g = b.space.gap;
    if (g.end - g.start) * w.sector < MIN_LINUX {
        bail!("the space for Linux on {} is under {} GB", w.disk, MIN_LINUX / GB);
    }
    if let Some(sh) = &b.space.shrink {
        let p = &sh.part;
        let size = sh.to.to_string();
        plan.expect(
            Cmd::new("ntfsresize").args(["--no-action", "--no-progress-bar", "--size", &size, &p.dev]),
            true,
            format!("ntfsresize refuses to shrink {}; boot Windows, shut it down fully and try again", p.dev),
        );
        plan.run(Cmd::new("ntfsresize").args(["--no-progress-bar", "--size", &size, &p.dev]).stdin("y\n"));
        // The partition keeps its start, GUID, name and NTFS boot sector; only its size changes.
        plan.run(
            Cmd::new("sfdisk")
                .args(["--no-reread", "--wipe-partitions", "never", "-N"])
                .arg(p.partn.to_string())
                .arg(&w.disk)
                .stdin(format!(", {}\n", sh.to / w.sector)),
        );
        plan.run(Cmd::new("partx").args(["-u", &w.disk]));
    }
    let (boot, root) = (w.next_partn, w.next_partn + 1);
    let boot_end = g.start + XBOOTLDR_SIZE / w.sector - 1;
    plan.run(
        Cmd::new("sgdisk")
            .arg(format!("-n{boot}:{}:{boot_end}", g.start))
            .arg(format!("-t{boot}:ea00"))
            .arg(format!("-c{boot}:boot"))
            .arg(format!("-n{root}:{}:{}", boot_end + 1, g.end - 1))
            .arg(format!("-t{root}:{root_code}"))
            .arg(format!("-c{root}:root"))
            .arg(&w.disk),
    );
    Ok((boot, root))
}

/// Install systemd-boot onto Windows' ESP mounted at `esp`, next to `EFI/Microsoft`, which it
/// never writes, reading kernels from the XBOOTLDR partition at `boot`. The menu shows, so Windows
/// Boot Manager, which systemd-boot finds on its own ESP, can be picked.
pub(crate) fn boot_manager(plan: &mut Plan, w: &WindowsDisk, esp: &str, boot: &str, efi_vars: bool) {
    plan.expect(
        Cmd::new("test").arg("-f").arg(format!("{esp}/EFI/Microsoft/Boot/bootmgfw.efi")),
        true,
        format!("{} holds no Windows Boot Manager; this is not Windows' ESP", w.esp.dev),
    );
    // With EFI variables, bootctl adds its firmware entry first in BootOrder and keeps Windows'.
    let mut bootctl =
        Cmd::new("bootctl").arg(format!("--esp-path={esp}")).arg(format!("--boot-path={boot}")).arg("install");
    if !efi_vars {
        bootctl = bootctl.arg("--no-variables");
    }
    plan.run(bootctl);
    let timeout = r#"grep -qs '^timeout' "$1" || echo 'timeout 5' >>"$1""#;
    plan.run(Cmd::new("sh").args(["-c", timeout, "sh"]).arg(format!("{esp}/loader/loader.conf")));
}

/// `C:` shrunk to `to` bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shrink {
    pub part: Part,
    pub to: u64,
}

/// Where Linux goes on a Windows disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Space {
    pub shrink: Option<Shrink>,
    pub gap: Gap,
}

impl Space {
    pub fn bytes(&self, sector: u64) -> u64 {
        (self.gap.end - self.gap.start) * sector
    }
}

/// Linux takes the largest free space when `take` is `None`, else `take` bytes shrunk off the end
/// of `C:`, together with whatever free space already follows it.
pub fn space(w: &WindowsDisk, take: Option<u64>) -> Result<Space> {
    let Some(take) = take else {
        let gap = w
            .gaps
            .iter()
            .copied()
            .max_by_key(|g| g.end - g.start)
            .filter(|g| (g.end - g.start) * w.sector >= MIN_LINUX);
        let Some(gap) = gap else {
            bail!("{} has no {} GB of free space; shrink Windows instead", w.disk, MIN_LINUX / GB)
        };
        return Ok(Space { shrink: None, gap });
    };
    let most = w.max_linux().map_err(anyhow::Error::msg)?;
    if take < MIN_LINUX || take > most {
        bail!("Linux can take {} to {} GB from Windows, not {} GB", MIN_LINUX / GB, most / GB, take / GB);
    }
    let p = &w.windows;
    let to = (p.size - take) / MIB * MIB;
    let end = p.start + w.sectors(to);
    let start = end.div_ceil(w.align()) * w.align();
    Ok(Space { shrink: Some(Shrink { part: p.clone(), to }), gap: Gap { start, end: w.windows_limit } })
}

/// Read a disk holding Windows from `sfdisk --json <disk>` and `lsblk -J -b -p -o NAME,FSTYPE,UUID
/// <disk>`, or `None` when it has no ESP and no `C:`. `shrink_min` comes from [`shrink_min`].
pub fn read_windows(sfdisk: &str, lsblk: &str, shrink_min: Result<u64, String>) -> Result<Option<WindowsDisk>> {
    let t: Value = serde_json::from_str(sfdisk).context("reading sfdisk's output")?;
    let t = &t["partitiontable"];
    if t["label"].as_str() != Some("gpt") {
        return Ok(None);
    }
    let l: Value = serde_json::from_str(lsblk).context("reading lsblk's output")?;
    let mut fs = std::collections::HashMap::new();
    let mut each = |v: &Value| {
        let name = v["name"].as_str().unwrap_or_default().to_string();
        let f =
            (v["fstype"].as_str().unwrap_or_default().to_string(), v["uuid"].as_str().unwrap_or_default().to_string());
        fs.insert(name, f);
    };
    for d in l["blockdevices"].as_array().into_iter().flatten() {
        each(d);
        d["children"].as_array().into_iter().flatten().for_each(&mut each);
    }
    let sector = t["sectorsize"].as_u64().unwrap_or(512);
    let disk = t["device"].as_str().unwrap_or_default().to_string();
    let mut parts: Vec<Part> = Vec::new();
    for p in t["partitions"].as_array().into_iter().flatten() {
        let dev = p["node"].as_str().unwrap_or_default().to_string();
        let (fstype, uuid) = fs.get(&dev).cloned().unwrap_or_default();
        let partn = dev.trim_start_matches(&disk).trim_start_matches('p').parse().unwrap_or_default();
        parts.push(Part {
            uuid,
            fstype,
            partuuid: p["uuid"].as_str().unwrap_or_default().to_lowercase(),
            parttype: p["type"].as_str().unwrap_or_default().to_lowercase(),
            label: p["name"].as_str().unwrap_or_default().to_string(),
            size: p["size"].as_u64().unwrap_or_default() * sector,
            start: p["start"].as_u64().unwrap_or_default(),
            partn,
            dev,
            ..Part::default()
        });
    }
    let esp = parts.iter().find(|p| p.parttype == ESP_TYPE && p.fstype == "vfat");
    let windows = parts
        .iter()
        .filter(|p| p.parttype == BASIC_DATA_TYPE && (p.fstype == "ntfs" || p.fstype.eq_ignore_ascii_case("bitlocker")))
        .max_by_key(|p| p.size);
    let (Some(esp), Some(windows)) = (esp, windows) else { return Ok(None) };

    let first = t["firstlba"].as_u64().unwrap_or(34);
    let last = t["lastlba"].as_u64().unwrap_or_default() + 1;
    let align = (MIB / sector).max(1);
    let mut used: Vec<(u64, u64)> = parts.iter().map(|p| (p.start, p.start + p.size / sector)).collect();
    used.sort();
    let mut gaps = Vec::new();
    let mut at = first;
    for &(s, e) in used.iter().chain([&(last, last)]) {
        let start = at.div_ceil(align) * align;
        let end = s / align * align;
        if end > start && (end - start) * sector >= MIB {
            gaps.push(Gap { start, end });
        }
        at = at.max(e);
    }
    let windows_end = windows.start + windows.size / sector;
    let limit = used.iter().map(|u| u.0).filter(|&s| s >= windows_end).min().unwrap_or(last);
    Ok(Some(WindowsDisk {
        disk,
        sector,
        esp: esp.clone(),
        windows: windows.clone(),
        windows_limit: limit,
        gaps,
        next_partn: parts.iter().map(|p| p.partn).max().unwrap_or_default() + 1,
        shrink_min,
    }))
}

/// The probe [`shrink_min`] reads.
pub fn ntfs_info(dev: &str) -> Cmd {
    Cmd::new("ntfsresize").args(["--info", "--no-progress-bar", dev])
}

fn bitlocker_refusal() -> String {
    format!(
        "Windows is encrypted with BitLocker. In Windows, shrink C: in Disk Management to free at least {} GB.",
        MIN_LINUX / GB
    )
}

/// The size `ntfsresize --info` says the volume can shrink to, or why Windows has to be booted
/// and shut down properly first.
pub fn shrink_min(status: i32, out: &str) -> Result<u64, String> {
    let lower = out.to_lowercase();
    if lower.contains("hibernat") || lower.contains("unsafe state") || lower.contains("fast restart") {
        return Err(
            "Windows did not fully shut down. In Windows, turn off Fast Startup in Power Options, then shut down."
                .into(),
        );
    }
    if lower.contains("scheduled for check") || lower.contains("chkdsk") {
        return Err(
            "Windows marked C: for a disk check. In Windows, run `chkdsk C: /f`, restart, then shut down.".into()
        );
    }
    let min = out.lines().find_map(|l| {
        let rest = l.trim().strip_prefix("You might resize at ")?;
        rest.split_whitespace().next()?.parse::<u64>().ok()
    });
    match min {
        Some(m) if status == 0 => Ok(m),
        _ => Err(format!(
            "ntfsresize cannot shrink Windows: {}",
            out.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("no output").trim()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SFDISK: &str = r#"{"partitiontable":{"label":"gpt","id":"X","device":"/dev/nvme0n1","unit":"sectors",
      "firstlba":34,"lastlba":1953525134,"sectorsize":512,"partitions":[
        {"node":"/dev/nvme0n1p1","start":2048,"size":204800,"type":"C12A7328-F81F-11D2-BA4B-00A0C93EC93B","uuid":"AAAA-1","name":"EFI system partition"},
        {"node":"/dev/nvme0n1p2","start":206848,"size":32768,"type":"E3C9E316-0B5C-4DB8-817D-F92DF00215AE","uuid":"BBBB-2","name":"Microsoft reserved partition"},
        {"node":"/dev/nvme0n1p3","start":239616,"size":1000000000,"type":"EBD0A0A2-B9E5-4433-87C0-68B6B72699C7","uuid":"CCCC-3","name":"Basic data partition"},
        {"node":"/dev/nvme0n1p4","start":1000239616,"size":1433600,"type":"DE94BBA4-06D1-4D40-A16A-BFD50179D6AC","uuid":"DDDD-4","name":"Basic data partition"}]}}"#;

    fn lsblk(c: &str) -> String {
        format!(
            r#"{{"blockdevices":[{{"name":"/dev/nvme0n1","fstype":null,"uuid":null,"children":[
          {{"name":"/dev/nvme0n1p1","fstype":"vfat","uuid":"1234-ABCD"}},
          {{"name":"/dev/nvme0n1p2","fstype":null,"uuid":null}},
          {{"name":"/dev/nvme0n1p3","fstype":"{c}","uuid":"0011"}},
          {{"name":"/dev/nvme0n1p4","fstype":"ntfs","uuid":"0022"}}]}}]}}"#
        )
    }

    fn disk() -> WindowsDisk {
        read_windows(SFDISK, &lsblk("ntfs"), Ok(200 * GIB)).unwrap().unwrap()
    }

    #[test]
    fn finds_windows_its_esp_and_the_free_space_at_the_end() {
        let w = disk();
        assert_eq!((w.esp.dev.as_str(), w.esp.uuid.as_str(), w.esp.partn), ("/dev/nvme0n1p1", "1234-ABCD", 1));
        assert_eq!((w.windows.dev.as_str(), w.windows.partn), ("/dev/nvme0n1p3", 3));
        assert_eq!(w.windows_limit, 1000239616);
        assert_eq!(w.next_partn, 5);
        // From the end of the recovery partition to the end of the disk, aligned to 1 MiB.
        assert_eq!(w.gaps, [Gap { start: 1001674752, end: 1953523712 }]);
        assert!(w.free() > 450 * GIB);
    }

    #[test]
    fn free_space_large_enough_is_used_without_touching_windows() {
        let s = space(&disk(), None).unwrap();
        assert_eq!(s.shrink, None);
        assert_eq!(s.gap, Gap { start: 1001674752, end: 1953523712 });
    }

    #[test]
    fn shrinking_frees_the_space_between_windows_and_its_recovery_partition() {
        let w = disk();
        let s = space(&w, Some(64 * GIB)).unwrap();
        let sh = s.shrink.as_ref().unwrap();
        assert_eq!(sh.to, (w.windows.size - 64 * GIB) / MIB * MIB);
        assert_eq!(s.gap.end, 1000239616);
        assert_eq!(s.gap.start % 2048, 0);
        assert!(s.bytes(512) >= 64 * GIB - MIB);
    }

    #[test]
    fn windows_keeps_room_beyond_what_ntfsresize_needs() {
        let w = disk();
        let most = w.max_linux().unwrap();
        assert_eq!(most, (w.windows.size - 220 * GIB) / GB * GB);
        assert!(space(&w, Some(most + GIB)).is_err());
        assert!(space(&w, Some(16 * GIB)).is_err());
        let full = read_windows(SFDISK, &lsblk("ntfs"), Ok(w.windows.size - 30 * GIB)).unwrap().unwrap();
        assert!(full.max_linux().unwrap_err().contains("too little free space"));
    }

    #[test]
    fn refuses_a_hibernated_a_dirty_and_a_bitlocker_volume() {
        let hib = "The NTFS partition is in an unsafe state. Please resume and shutdown Windows fully (no hibernation or fast restarting)";
        assert!(shrink_min(1, hib).unwrap_err().contains("Fast Startup"));
        assert!(shrink_min(1, "ERROR: The NTFS partition is hibernated.").unwrap_err().contains("Fast Startup"));
        let dirty = "Volume is scheduled for check.\nRun chkdsk /f and please try again, or see option -f.";
        assert!(shrink_min(1, dirty).unwrap_err().contains("chkdsk C: /f"));
        let ok = "ntfsresize v2022.10.3\nCluster size : 4096 bytes\nYou might resize at 214748364800 bytes or 214749 MB (freeing 297251 MB).\n";
        assert_eq!(shrink_min(0, ok), Ok(214748364800));
        let w = read_windows(SFDISK, &lsblk("BitLocker"), Ok(GIB)).unwrap().unwrap();
        assert!(w.bitlocker());
        assert!(w.max_linux().unwrap_err().contains("BitLocker"));
        assert!(space(&w, Some(64 * GIB)).unwrap_err().to_string().contains("Disk Management"));
        assert!(space(&w, None).is_ok());
    }

    #[test]
    fn a_disk_without_windows_is_not_read_as_one() {
        let none = SFDISK.replace("EBD0A0A2", "0FC63DAF");
        assert_eq!(read_windows(&none, &lsblk("ntfs"), Ok(0)).unwrap(), None);
    }
}
