//! `fspec try`: boot a distro's live ISO in a throwaway KVM virtual machine, on this Linux host or
//! over ssh on another, without touching either host's system state. Everything a run needs lives
//! in one bash script, so the host that runs the VM needs qemu and OVMF, not this fspec.

use std::cmp::Ordering;

use anyhow::{Context, Result, bail};
use fspec_core::fetch::Fetch;
use fspec_core::runner::Cmd;

#[cfg(test)]
mod tests;
pub mod vm;

/// The distros `fspec try` knows by name, with what their default ISO boots into.
pub const DISTROS: [(&str, &str); 8] = [
    ("tumbleweed", "openSUSE Tumbleweed, KDE Plasma live"),
    ("fedora", "Fedora Workstation live, GNOME"),
    ("ubuntu", "Ubuntu desktop, the current LTS"),
    ("debian", "Debian stable live, GNOME"),
    ("arch", "Arch Linux install ISO, console only"),
    ("nixos", "NixOS unstable graphical installer"),
    ("aerynos", "AerynOS live, GNOME"),
    ("void", "Void Linux live, Xfce"),
];

const ALIASES: [(&str, &str); 3] = [("opensuse", "tumbleweed"), ("aeryn", "aerynos"), ("archlinux", "arch")];

/// What the user asked to boot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Distro(&'static str),
    Url(String),
    /// A path on the host that runs the VM.
    Path(String),
}

pub fn target(arg: &str) -> Result<Target> {
    if arg.starts_with("https://") || arg.starts_with("http://") {
        return Ok(Target::Url(arg.to_string()));
    }
    if arg.contains('/') || arg.ends_with(".iso") {
        return Ok(Target::Path(arg.to_string()));
    }
    let name = ALIASES.iter().find(|(a, _)| *a == arg).map_or(arg, |(_, d)| d);
    match DISTROS.iter().find(|(d, _)| *d == name) {
        Some((d, _)) => Ok(Target::Distro(d)),
        None => bail!(
            "`{arg}` is not a distro fspec try knows ({}), an http(s) URL or a path to an ISO",
            DISTROS.map(|(d, _)| d).join(", ")
        ),
    }
}

/// An ISO to download into the cache on the VM's host, under `iso/<group>/<file>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Iso {
    pub group: String,
    pub url: String,
    pub file: String,
    pub sha256: Option<String>,
}

impl Iso {
    pub fn from_url(url: &str) -> Result<Iso> {
        let path = url.split(['?', '#']).next().unwrap_or(url);
        let file = path.rsplit('/').next().filter(|f| !f.is_empty()).context("the URL does not end in a file name")?;
        Ok(Iso { group: "url".into(), url: url.into(), file: file.into(), sha256: None })
    }
}

const TUMBLEWEED: &str = "https://download.opensuse.org/tumbleweed/iso/openSUSE-Tumbleweed-KDE-Live-x86_64-Current.iso";
const FEDORA: &str = "https://fedoraproject.org/releases.json";
const UBUNTU_LTS: &str = "https://changelogs.ubuntu.com/meta-release-lts";
const UBUNTU: &str = "https://releases.ubuntu.com";
const DEBIAN: &str = "https://cdimage.debian.org/debian-cd/current-live/amd64/iso-hybrid";
const ARCH: &str = "https://geo.mirror.pkgbuild.com/iso/latest";
const NIXOS: &str = "https://channels.nixos.org/nixos-unstable/latest-nixos-graphical-x86_64-linux.iso";
const AERYNOS: &str = "https://aerynos.com/download/";
const VOID: &str = "https://repo-default.voidlinux.org/live/current";

/// The distro's current ISO, from the checksum list or release index its project publishes. A
/// stable alias URL is downloaded where the project has one; the versioned name it stands for
/// names the cached file, so a new release downloads again and replaces the old one.
pub fn resolve(fetch: &dyn Fetch, distro: &str) -> Result<Iso> {
    let text = |url: &str| -> Result<String> { Ok(String::from_utf8_lossy(&fetch.get(url)?).into_owned()) };
    let iso = |url: String, file: String, sha256: Option<String>| Iso { group: distro.into(), url, file, sha256 };
    let missing = |what: &str, url: &str| anyhow::anyhow!("no {what} listed in {url}");
    match distro {
        "tumbleweed" => {
            let sums_url = format!("{TUMBLEWEED}.sha256");
            let sums = text(&sums_url)?;
            let file = pick(&sums, "openSUSE-Tumbleweed-KDE-Live-x86_64-Snapshot", ".iso")
                .ok_or_else(|| missing("KDE live ISO", &sums_url))?;
            Ok(iso(TUMBLEWEED.into(), file.clone(), sum(&sums, &file)))
        }
        "fedora" => {
            let releases = fetch.get_json(FEDORA)?;
            let best = releases
                .as_array()
                .into_iter()
                .flatten()
                .filter(|r| r["variant"] == "Workstation" && r["arch"] == "x86_64")
                .filter_map(|r| {
                    let version = r["version"].as_str()?.parse::<u32>().ok()?;
                    let link = r["link"].as_str().filter(|l| l.ends_with(".iso"))?;
                    Some((version, link, r["sha256"].as_str()))
                })
                .max_by_key(|(v, ..)| *v)
                .ok_or_else(|| missing("Workstation live ISO", FEDORA))?;
            let file = best.1.rsplit('/').next().unwrap_or(best.1).to_string();
            Ok(iso(best.1.into(), file, best.2.map(String::from)))
        }
        "ubuntu" => {
            let meta = text(UBUNTU_LTS)?;
            let dist = meta
                .lines()
                .filter_map(|l| l.strip_prefix("Dist:"))
                .map(str::trim)
                .next_back()
                .ok_or_else(|| missing("LTS release", UBUNTU_LTS))?;
            let sums_url = format!("{UBUNTU}/{dist}/SHA256SUMS");
            let sums = text(&sums_url)?;
            let file = pick(&sums, "ubuntu-", "-desktop-amd64.iso").ok_or_else(|| missing("desktop ISO", &sums_url))?;
            Ok(iso(format!("{UBUNTU}/{dist}/{file}"), file.clone(), sum(&sums, &file)))
        }
        "debian" | "arch" | "void" => {
            let (base, list, prefix, suffix) = match distro {
                "debian" => (DEBIAN, "SHA256SUMS", "debian-live-", "-amd64-gnome.iso"),
                "arch" => (ARCH, "sha256sums.txt", "archlinux-2", "-x86_64.iso"),
                _ => (VOID, "sha256sum.txt", "void-live-x86_64-2", "-xfce.iso"),
            };
            let sums_url = format!("{base}/{list}");
            let sums = text(&sums_url)?;
            let file = pick(&sums, prefix, suffix).ok_or_else(|| missing("live ISO", &sums_url))?;
            Ok(iso(format!("{base}/{file}"), file.clone(), sum(&sums, &file)))
        }
        "nixos" => {
            let sums_url = format!("{NIXOS}.sha256");
            let sums = text(&sums_url)?;
            let file = pick(&sums, "nixos-graphical-", "-x86_64-linux.iso")
                .ok_or_else(|| missing("graphical ISO", &sums_url))?;
            Ok(iso(NIXOS.into(), file.clone(), sum(&sums, &file)))
        }
        "aerynos" => {
            let page = text(AERYNOS)?;
            let url = pick(&page, "https://cdn.aerynos.dev/isos/AerynOS-", "-GNOME-live-x86_64.iso")
                .ok_or_else(|| missing("GNOME live ISO", AERYNOS))?;
            let file = url.rsplit('/').next().unwrap_or(&url).to_string();
            let sha256 = text(&format!("{url}.sha256sum")).ok().and_then(|s| sum(&s, &file));
            Ok(iso(url, file, sha256))
        }
        _ => bail!("fspec try has no ISO source for `{distro}`"),
    }
}

fn tokens(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| c.is_whitespace() || "\"'<>()*=,".contains(c)).filter(|t| !t.is_empty())
}

/// The newest token of `text` that starts with `prefix` and ends with `suffix`.
fn pick(text: &str, prefix: &str, suffix: &str) -> Option<String> {
    tokens(text).filter(|t| t.starts_with(prefix) && t.ends_with(suffix)).max_by(|a, b| natural(a, b)).map(String::from)
}

/// The SHA-256 a checksum list gives `file`, in either `hash  file` or `SHA256 (file) = hash` form.
pub(crate) fn sum(text: &str, file: &str) -> Option<String> {
    text.lines().filter(|l| tokens(l).any(|t| t == file)).find_map(|l| {
        tokens(l).find(|t| t.len() == 64 && t.chars().all(|c| c.is_ascii_hexdigit())).map(str::to_lowercase)
    })
}

/// Compares runs of digits by value, so `ubuntu-26.10` sorts after `ubuntu-26.4`.
fn natural(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a, b);
    loop {
        match (a.chars().next(), b.chars().next()) {
            (None, None) => return Ordering::Equal,
            (None, _) => return Ordering::Less,
            (_, None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let da = a.find(|c: char| !c.is_ascii_digit()).unwrap_or(a.len());
                let db = b.find(|c: char| !c.is_ascii_digit()).unwrap_or(b.len());
                let (na, nb) = (a[..da].trim_start_matches('0'), b[..db].trim_start_matches('0'));
                let o = na.len().cmp(&nb.len()).then_with(|| na.cmp(nb));
                if o != Ordering::Equal {
                    return o;
                }
                (a, b) = (&a[da..], &b[db..]);
            }
            (Some(x), Some(y)) => {
                if x != y {
                    return x.cmp(&y);
                }
                (a, b) = (&a[x.len_utf8()..], &b[y.len_utf8()..]);
            }
        }
    }
}

/// What the VM boots from besides its disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Media {
    Download(Iso),
    /// An ISO already on the VM's host.
    Path(String),
    /// A kept VM booting from what was installed on its disk.
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Display {
    /// A GTK window with virgl, on the desktop of the host that runs the VM.
    Window,
    /// No window: a password-protected VNC server on a unix socket in the run directory.
    Vnc,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Vm {
    /// Names the run directory `/tmp/fspec-try-<id>` and a throwaway disk.
    pub id: String,
    /// The distro or kept VM name, shown as the qemu process and window name.
    pub label: String,
    pub media: Media,
    /// Keep the disk and UEFI variables under this name instead of deleting them on exit.
    pub keep: Option<String>,
    /// Default: half the host's threads, between 2 and 16.
    pub cpus: Option<u32>,
    /// Default: a third of the host's memory, between 4 and 16 GiB.
    pub memory_gib: Option<u32>,
    pub disk_gib: u32,
    pub display: Display,
    /// The VNC password, at most 8 characters as VNC authentication takes.
    pub password: String,
}

/// Days a cached ISO stays after the last run that booted it.
pub const ISO_DAYS: u32 = 7;

pub fn run_dir(id: &str) -> String {
    format!("/tmp/fspec-try-{id}")
}

/// The VNC socket a [`Display::Vnc`] run listens on.
pub fn vnc_socket(id: &str) -> String {
    format!("{}/vnc.sock", run_dir(id))
}

/// Firmware pairs (code, variables) as distros ship them: openSUSE, Arch, Debian and Ubuntu,
/// Fedora, then older Debian. Each code file needs the variables file of its own size.
pub(crate) const OVMF: [(&str, &str); 5] = [
    ("/usr/share/qemu/ovmf-x86_64-4m-code.bin", "/usr/share/qemu/ovmf-x86_64-4m-vars.bin"),
    ("/usr/share/edk2/x64/OVMF_CODE.4m.fd", "/usr/share/edk2/x64/OVMF_VARS.4m.fd"),
    ("/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_VARS_4M.fd"),
    ("/usr/share/edk2/ovmf/OVMF_CODE.fd", "/usr/share/edk2/ovmf/OVMF_VARS.fd"),
    ("/usr/share/OVMF/OVMF_CODE.fd", "/usr/share/OVMF/OVMF_VARS.fd"),
];

/// The qemu command, run from inside the run directory, where the script links the firmware, the
/// disk, the ISO and the variables under fixed names. `$cpus`, `$mem`, `$dcache` and `$aio` are
/// the script's. The disk boots before the ISO: OVMF skips an empty disk, so a new VM boots the ISO
/// and one whose installer rebooted it boots what was installed.
pub fn qemu(vm: &Vm) -> Cmd {
    let mut c = Cmd::new("qemu-system-x86_64").args([
        "-name",
        &format!("fspec-try-{}", vm.label),
        "-machine",
        "q35,accel=kvm",
        "-cpu",
        "host",
        "-smp",
        "$cpus",
        "-m",
        "${mem}M",
        "-drive",
        "if=pflash,format=raw,readonly=on,file=code.fd",
        "-drive",
        "if=pflash,format=raw,file=vars.fd",
        "-object",
        "iothread,id=io0",
        "-drive",
        "if=none,id=disk,format=qcow2,discard=unmap,cache=$dcache,aio=$aio,file=disk.qcow2",
        "-device",
        "virtio-blk-pci,drive=disk,iothread=io0,bootindex=0",
    ]);
    if vm.media != Media::None {
        c = c.args([
            "-drive",
            "if=none,id=cd,media=cdrom,readonly=on,file=cdrom.iso",
            "-device",
            "ide-cd,drive=cd,bootindex=1",
        ]);
    }
    c = c.args([
        "-nic",
        "user,model=virtio-net-pci",
        "-device",
        "qemu-xhci",
        "-device",
        "usb-tablet",
        "-device",
        "virtio-rng-pci",
        "-qmp",
        "unix:qmp.sock,server=on,wait=off",
    ]);
    match vm.display {
        Display::Window => c.args(["-device", "virtio-vga-gl", "-display", "gtk,gl=on"]),
        Display::Vnc => c.args([
            "-device",
            "virtio-vga",
            "-display",
            "none",
            "-object",
            "secret,id=vncpw,file=vnc.pw",
            "-vnc",
            "unix:vnc.sock,password-secret=vncpw",
        ]),
    }
}

/// A word for bash: quoted, except that one referring to a script variable expands.
pub(crate) fn word(a: &str) -> String {
    if a.contains('$') { format!("\"{a}\"") } else { shell_words::quote(a).into_owned() }
}

pub(crate) fn q(a: &str) -> String {
    shell_words::quote(a).into_owned()
}

/// The script that boots `vm` and, on any exit, stops qemu and deletes the run directory and a
/// throwaway disk. It expects [`stage`] to have created the run directory.
pub fn script(vm: &Vm) -> String {
    let run = run_dir(&vm.id);
    let mut s = String::from("#!/usr/bin/env bash\nset -eu\numask 077\n");
    s += &format!("id={}\nrun={}\n", q(&vm.id), q(&run));
    s += "cache=\"${XDG_CACHE_HOME:-$HOME/.cache}/fspec/try\"\ndata=\"${XDG_DATA_HOME:-$HOME/.local/share}/fspec/try\"\n";
    s += "say() { printf 'fspec try: %s\\n' \"$*\" >&2; }\n";
    s += "die() { say \"$*\"; exit 1; }\n";
    s += "echo $$ > \"$run/pid\"\n";
    let disk = match &vm.keep {
        Some(name) => {
            s += &format!("printf %s {} > \"$run/keep\"\n", q(name));
            format!("$data/{name}/disk.qcow2")
        }
        None => "$cache/disks/$id.qcow2".to_string(),
    };
    s += &format!(
        "q=\ncleanup() {{\n  if [ -n \"$q\" ]; then kill \"$q\" 2>/dev/null || true; wait \"$q\" 2>/dev/null || true; fi\n  rm -rf \"$run\"{}\n}}\ntrap cleanup EXIT\ntrap 'exit 130' INT HUP TERM\n",
        if vm.keep.is_none() { " \"$cache/disks/$id.qcow2\"" } else { "" }
    );
    s += "command -v qemu-system-x86_64 >/dev/null || die \"qemu-system-x86_64 is not installed on $(hostname)\"\n";
    s += "[ -w /dev/kvm ] || die \"/dev/kvm is missing or not writable for $(id -un) on $(hostname)\"\n";
    // Runs whose script died without its trap (a power cut, kill -9) leave their directory and
    // disk behind. A directory without a pid file belongs to a run that is still being staged.
    s += r#"mkdir -p "$cache/disks" "$cache/iso"
for d in /tmp/fspec-try-*; do
  [ -d "$d" ] && [ -O "$d" ] && [ "$d" != "$run" ] || continue
  p=$(cat "$d/pid" 2>/dev/null || true)
  if [ -n "$p" ]; then kill -0 "$p" 2>/dev/null && continue
  elif [ -z "$(find "$d" -maxdepth 0 -mmin +60)" ]; then continue; fi
  rm -rf "$d" "$cache/disks/${d#/tmp/fspec-try-}.qcow2"
done
"#;
    s += "code=\nvars=\n";
    for (c, v) in OVMF {
        s += &format!("[ -n \"$code\" ] || {{ [ -f {c} ] && [ -f {v} ] && code={c} vars={v}; }} || true\n");
    }
    s += "[ -n \"$code\" ] || die \"no OVMF firmware on $(hostname); install ovmf or edk2-ovmf\"\n";
    s += "ln -s \"$code\" \"$run/code.fd\"\n";
    match &vm.media {
        Media::Download(iso) => {
            let dir = format!("$cache/iso/{}", iso.group);
            let file = q(&iso.file);
            let url = q(&iso.url);
            s += &format!(
                "iso=\"{dir}\"/{file}\nif [ ! -f \"$iso\" ]; then\n  mkdir -p \"{dir}\"\n  say \"downloading {}\"\n",
                iso.file
            );
            s += &format!(
                "  if command -v curl >/dev/null; then curl -fL --retry 3 -C - --progress-bar -o \"$iso.part\" {url}\n  else wget -c -q --show-progress -O \"$iso.part\" {url}; fi\n"
            );
            if let Some(sha) = &iso.sha256 {
                s += &format!(
                    "  echo \"{sha}  $iso.part\" | sha256sum -c --status - || {{ rm -f \"$iso.part\"; die \"{} does not match its published checksum; run again\"; }}\n",
                    iso.file
                );
            }
            if iso.group != "url" {
                s += &format!(
                    "  find \"{dir}\" -maxdepth 1 -type f ! -name {file} ! -name {}.part -delete\n",
                    q(&iso.file)
                );
            }
            s += "  mv \"$iso.part\" \"$iso\"\nfi\ntouch \"$iso\"\nln -s \"$iso\" \"$run/cdrom.iso\"\n";
        }
        Media::Path(p) => {
            s += &format!("[ -f {0} ] || die \"no ISO at \"{0}\nln -s \"$(realpath {0})\" \"$run/cdrom.iso\"\n", q(p));
        }
        Media::None => {}
    }
    // A run touches the cached ISO it boots, so an unused one is older than both its atime and
    // mtime say; one a running VM boots stays whatever its age.
    s += &format!(
        r#"live=$(for d in /tmp/fspec-try-*; do [ -d "$d" ] && kill -0 "$(cat "$d/pid" 2>/dev/null)" 2>/dev/null && readlink -f "$d/cdrom.iso"; done; true)
find "$cache/iso" -type f -name '*.iso' -mmin +{0} -amin +{0} | while IFS= read -r f; do
  printf '%s\n' "$live" | grep -qxF "$f" || {{ say "deleting $f, unused for {1} days"; rm -f "$f"; }}
done
"#,
        ISO_DAYS * 24 * 60,
        ISO_DAYS
    );
    let size = vm.disk_gib;
    match &vm.keep {
        Some(name) => {
            s += &format!("keep=\"$data\"/{}\n", q(name));
            if vm.media == Media::None {
                s += &format!("[ -f \"$keep/disk.qcow2\" ] || die \"no kept VM named \"{}\n", q(name));
            }
            s += "mkdir -p \"$keep\"\n";
            s += &format!(
                "[ -f \"$keep/disk.qcow2\" ] || qemu-img create -q -f qcow2 -o nocow=on \"$keep/disk.qcow2\" {size}G\n"
            );
            s += "[ -f \"$keep/vars.fd\" ] || cp \"$vars\" \"$keep/vars.fd\"\n";
            s += "ln -s \"$keep/disk.qcow2\" \"$run/disk.qcow2\"\nln -s \"$keep/vars.fd\" \"$run/vars.fd\"\n";
        }
        None => {
            s += &format!(
                "qemu-img create -q -f qcow2 -o nocow=on \"{disk}\" {size}G\nln -s \"{disk}\" \"$run/disk.qcow2\"\n"
            );
            s += "cp \"$vars\" \"$run/vars.fd\"\n";
        }
    }
    s += &disk_probe("$run/disk.qcow2", vm.keep.is_some());
    s += &match vm.cpus {
        Some(n) => format!("cpus={n}\n"),
        None => "cpus=$(( $(nproc) / 2 ))\n[ \"$cpus\" -le 16 ] || cpus=16\n[ \"$cpus\" -ge 2 ] || cpus=2\n".into(),
    };
    s += &match vm.memory_gib {
        Some(g) => format!("mem={}\n", u64::from(g) * 1024),
        None => "mem=$(( $(awk '/^MemTotal:/ { print int($2 / 1024) }' /proc/meminfo) / 3 ))\n[ \"$mem\" -le 16384 ] || mem=16384\n[ \"$mem\" -ge 4096 ] || mem=4096\n".into(),
    };
    if vm.display == Display::Vnc {
        s += &format!("printf %s {} > \"$run/vnc.pw\"\n", q(&vm.password));
    }
    let qemu = qemu(vm);
    s += "cd \"$run\"\n";
    s += &format!("{} \\\n", qemu.program);
    let args: Vec<String> = qemu.args.iter().map(|a| word(a)).collect();
    for pair in args.chunks(2) {
        s += &format!("  {} \\\n", pair.join(" "));
    }
    s += "  &\nq=$!\n";
    let gone = match &vm.keep {
        Some(_) => "the VM stops, its disk stays".to_string(),
        None => "the VM and its disk are deleted".to_string(),
    };
    s += &format!(
        "say \"{} is running ($cpus CPUs, $((mem / 1024)) GiB); Ctrl-C here or power it off and {gone}\"\n",
        vm.label
    );
    s += "wait \"$q\" || true\nq=\n";
    s
}

/// Sets `$dcache` and `$aio` for the disk at `disk`. A throwaway disk is deleted on exit, so it can
/// skip the guest's flushes; a kept one bypasses the host page cache where the filesystem takes
/// O_DIRECT.
pub(crate) fn disk_probe(disk: &str, kept: bool) -> String {
    let mut s = if kept {
        format!("dcache=none\nqemu-img bench -c 1 -t none -f qcow2 \"{disk}\" >/dev/null 2>&1 || dcache=writeback\n")
    } else {
        "dcache=unsafe\n".into()
    };
    s += &format!(
        "aio=io_uring\nqemu-img bench -c 1 -t \"$dcache\" -i io_uring -f qcow2 \"{disk}\" >/dev/null 2>&1 || aio=threads\n"
    );
    s
}

pub fn script_path(id: &str) -> String {
    format!("{}/try.sh", run_dir(id))
}

/// Creates the run directory on the VM's host and writes the script from stdin into it.
pub fn stage(id: &str) -> String {
    format!("mkdir -m 700 {} && cat > {}", run_dir(id), script_path(id))
}

/// Runs a staged script on `alias` with the VNC socket forwarded to `port` here. The far end gets a
/// pty even when this end has no terminal, so a dropped connection hangs the script up and its
/// trap deletes the VM.
pub fn remote_boot(alias: &str, id: &str, port: u16) -> Cmd {
    Cmd::new("ssh")
        .args(["-tt", "-o", "ExitOnForwardFailure=yes", "-o", "ServerAliveInterval=15", "-L"])
        .arg(format!("127.0.0.1:{port}:{}", vnc_socket(id)))
        .arg(alias)
        .arg(format!("bash {}", script_path(id)))
        .interactive()
}

/// A script printing the kept VMs, running VMs and cached ISOs on the VM's host.
pub const LIST: &str = r#"cache="${XDG_CACHE_HOME:-$HOME/.cache}/fspec/try"
data="${XDG_DATA_HOME:-$HOME/.local/share}/fspec/try"
echo "kept VMs in $data:"
for d in "$data"/*/; do [ -d "$d" ] && printf '  %s\t%s\n' "$(basename "$d")" "$(du -sh "$d" | cut -f1)"; done
echo "running:"
for d in /tmp/fspec-try-*; do
  [ -d "$d" ] && kill -0 "$(cat "$d/pid" 2>/dev/null)" 2>/dev/null || continue
  printf '  %s\t%s\n' "${d#/tmp/fspec-try-}" "$(cat "$d/keep" 2>/dev/null || echo throwaway)"
done
echo "cached ISOs in $cache/iso:"
for f in "$cache"/iso/*/*.iso; do [ -f "$f" ] && printf '  %s\t%s\n' "${f#$cache/iso/}" "$(du -h "$f" | cut -f1)"; done
true
"#;

/// A script deleting the kept VM `name`, refusing while it runs.
pub fn delete(name: &str) -> Result<String> {
    if !valid_name(name) {
        bail!("`{name}` is not a VM name");
    }
    let name = q(name);
    Ok(format!(
        r#"data="${{XDG_DATA_HOME:-$HOME/.local/share}}/fspec/try"
[ -d "$data"/{name} ] || {{ echo "fspec try: no kept VM named "{name} >&2; exit 1; }}
for d in /tmp/fspec-try-*; do
  [ "$(cat "$d/keep" 2>/dev/null)" = {name} ] && kill -0 "$(cat "$d/pid" 2>/dev/null)" 2>/dev/null && {{ echo "fspec try: "{name}" is running; stop it first" >&2; exit 1; }}
done
rm -rf "$data"/{name}
"#
    ))
}

/// What opens the VNC display on the machine fspec runs on, if it has a viewer: Screen Sharing on
/// macOS, else remote-viewer or a TigerVNC-style vncviewer found by `have`.
pub fn viewer(os: &str, port: u16, password: &str, have: impl Fn(&str) -> bool) -> Option<Cmd> {
    let url = format!("vnc://:{password}@127.0.0.1:{port}");
    if os == "macos" {
        return Some(Cmd::new("open").arg(url));
    }
    if have("remote-viewer") {
        return Some(Cmd::new("remote-viewer").arg(url));
    }
    have("vncviewer").then(|| Cmd::new("vncviewer").arg(format!("127.0.0.1::{port}")))
}

/// Kept VM names become directory names on the VM's host.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) && !name.starts_with('.')
}
