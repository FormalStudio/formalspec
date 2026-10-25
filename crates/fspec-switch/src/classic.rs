use std::path::Path;

use anyhow::{Result, bail};
use fspec_core::Paths;
use fspec_core::runner::Cmd;

use crate::machine::{BTRFS_OPTS, LOADER_ESP, Layout, Machine, SHARED_BOOT};
use crate::recipe::{Boot, boot_files, bootstrap, cmdline_file, misfit, populate, recipe, upgrade};
use crate::{Plan, subvol};

/// What `switch --distro` builds. The host's `system` already declares `distro`, so the apply
/// inside the new root installs the packages that block names.
pub struct Target<'a> {
    pub distro: &'a str,
    pub host: &'a str,
    /// `x86_64` or `aarch64`.
    pub arch: &'a str,
    /// Fedora release number, or Debian or Ubuntu suite. Fedora needs it.
    pub release: Option<&'a str>,
    pub spec_dir: &'a Path,
    pub spec_file: &'a str,
    /// The running `fspec`, copied into the new root to run the apply there.
    pub exe: &'a Path,
}

pub(crate) fn s(p: &Path) -> String {
    p.display().to_string()
}

pub(crate) struct Mounts {
    pub top: String,
    pub root: String,
}

/// Where a switch mounts the btrfs top level and the new root.
pub(crate) const WORK: &str = "/run/fspec/switch";

pub(crate) fn mounts(paths: &Paths, distro: &str) -> Mounts {
    Mounts { top: s(&paths.system(format!("{WORK}/top"))), root: s(&paths.system(format!("{WORK}/{distro}"))) }
}

/// The boot menu name of `distro`'s entry, e.g. `Void (fspec)`.
pub fn title(distro: &str) -> String {
    let mut c = distro.chars();
    let first = c.next().map(|f| f.to_uppercase().collect::<String>()).unwrap_or_default();
    format!("{first}{} (fspec)", c.as_str())
}

/// The options of `cmdline` that belong to the machine rather than to a root, like `console=` and
/// `net.ifnames=0`, and the extra mount options of its `rootflags=`.
fn carried(cmdline: &str) -> (Vec<&str>, Vec<&str>) {
    let (mut rest, mut flags) = (Vec::new(), Vec::new());
    for arg in cmdline.split_whitespace() {
        let key = arg.split('=').next().unwrap_or(arg);
        match key {
            "BOOT_IMAGE" | "root" | "initrd" | "ro" | "rw" | "rd.luks.uuid" | "rd.luks.name" | "cryptdevice" => {}
            // Each names the running root: its machine ID, its moss state, the host ID ZFSBootMenu
            // passes when it boots a pool.
            "systemd.machine_id" | "moss.fstx" | "spl.spl_hostid" => {}
            "rootflags" => flags.extend(
                arg["rootflags=".len()..]
                    .split(',')
                    .filter(|f| !f.is_empty() && !f.starts_with("subvol=") && !f.starts_with("subvolid=")),
            ),
            _ => rest.push(arg),
        }
    }
    (rest, flags)
}

/// The kernel command line of the root `distro` on `layout`, keeping the machine's options from
/// `cmdline`. On ZFS, ZFSBootMenu adds the root itself.
pub(crate) fn kernel_options(layout: &Layout, distro: &str, cmdline: &str) -> String {
    let (rest, extra) = carried(cmdline);
    let mut out = Vec::new();
    if let Some(b) = layout.btrfs() {
        let mut flags = vec![format!("subvol={}", subvol(distro))];
        flags.extend(extra.iter().map(|f| f.to_string()));
        out.push(format!("root=UUID={} rootflags={} rw", b.uuid, flags.join(",")));
        if let Some(luks) = &b.luks {
            out.push(format!("rd.luks.uuid={luks}"));
        }
    }
    out.extend(rest.iter().map(|a| a.to_string()));
    out.join(" ")
}

pub(crate) fn fstab(layout: &Layout, distro: &str) -> String {
    let mut text = String::new();
    if let Some(b) = layout.btrfs() {
        let u = &b.uuid;
        let home = b.home.trim_start_matches('/');
        text.push_str(&format!("UUID={u} / btrfs subvol={},compress=zstd 0 0\n", subvol(distro)));
        text.push_str(&format!("UUID={u} /home btrfs subvol={home},compress=zstd 0 0\n"));
        text.push_str(&format!("UUID={u} /opt/fspec btrfs subvol=@fspec,compress=zstd 0 0\n"));
        text.push_str(&format!("UUID={u} /swap btrfs subvol=@swap 0 0\n"));
        text.push_str("/swap/swapfile none swap defaults,nofail 0 0\n");
    }
    match layout {
        Layout::Default { esp, loader, .. } => {
            text.push_str(&format!("UUID={} /efi vfat umask=0077 0 2\n", esp.uuid));
            if let Some(l) = loader {
                text.push_str(&format!("UUID={} {LOADER_ESP} vfat umask=0077 0 2\n", l.uuid));
            }
        }
        Layout::Zfs { esp, .. } => {
            text.push_str(&format!("UUID={} /efi vfat umask=0077 0 2\n", esp.uuid));
        }
        Layout::Bios { boot, .. } => {
            text.push_str(&format!("UUID={} {SHARED_BOOT} ext4 defaults 0 2\n", boot.uuid));
            text.push_str(&format!("{SHARED_BOOT}/{distro} /boot none bind 0 0\n"));
        }
        Layout::Unsupported { .. } => {}
    }
    text
}

/// Writes the shared GRUB menu of the BIOS layout: an entry per root folder that holds a kernel
/// command line, using the stable kernel names each root keeps there. Fedora's grub2-install
/// looks for it under grub2/ and everyone else's under grub/, so both get a copy.
pub(crate) const GRUB_MENU: &str = r#"set -eu
boot=$1 uuid=$2 default=$3
mkdir -p "$boot/grub" "$boot/grub2"
{
	echo "set timeout=5"
	echo "set default=\"fspec-$default\""
	if grep -qs 'console=ttyS' "$boot"/*/fspec-cmdline; then
		echo "serial --unit=0 --speed=115200"
		echo "terminal_input console serial"
		echo "terminal_output console serial"
	fi
	for dir in "$boot"/*/; do
		d=$(basename "$dir")
		[ -f "$dir/fspec-cmdline" ] || continue
		kernel= initrd=
		for k in vmlinuz-linux vmlinuz; do if [ -e "$dir/$k" ]; then kernel=$k; break; fi; done
		for i in initramfs-linux.img initrd.img initramfs.img; do if [ -e "$dir/$i" ]; then initrd=$i; break; fi; done
		if [ -z "$kernel" ] || [ -z "$initrd" ]; then continue; fi
		title="$(printf %s "$d" | cut -c1 | tr a-z A-Z)$(printf %s "$d" | cut -c2-) (fspec)"
		printf "menuentry '%s' --id fspec-%s {\n\tsearch --no-floppy --fs-uuid --set=root %s\n\tlinux /%s/%s %s\n\tinitrd /%s/%s\n}\n" \
			"$title" "$d" "$uuid" "$d" "$kernel" "$(cat "$dir/fspec-cmdline")" "$d" "$initrd"
	done
} >"$boot/grub/grub.cfg.new"
cp "$boot/grub/grub.cfg.new" "$boot/grub2/grub.cfg.new"
mv "$boot/grub2/grub.cfg.new" "$boot/grub2/grub.cfg"
mv "$boot/grub/grub.cfg.new" "$boot/grub/grub.cfg"
"#;

pub(crate) fn grub_menu(boot_dir: &str, uuid: &str, default: &str) -> Cmd {
    Cmd::new("sh").args(["-c", GRUB_MENU, "sh", boot_dir, uuid, default])
}

/// Makes `default`'s root the one systemd-boot starts, and drops an EFI variable that would
/// outrank `loader.conf`.
pub(crate) fn sd_default(esp_dir: &str, default: &str) -> Vec<Cmd> {
    let conf = format!("{esp_dir}/loader/loader.conf");
    // Plain sh, since AerynOS's base has no sed or grep.
    let edit = r#"mkdir -p "${1%/*}" && touch "$1" && { while IFS= read -r l || [ -n "$l" ]; do case $l in default | default[[:space:]]*) ;; *) printf '%s\n' "$l" ;; esac; done <"$1"; echo "default $2-*"; } >"$1.new" && mv "$1.new" "$1""#;
    vec![
        Cmd::new("sh").args(["-c", edit, "sh", &conf, default]),
        Cmd::new("sh").args(["-c", "if command -v bootctl >/dev/null; then bootctl set-default '' || true; fi"]),
    ]
}

/// Host-side paths the new root gets bound in or copied from.
pub(crate) struct Sources {
    pub home: String,
    pub fspec: String,
    pub esp: String,
    pub shared_boot: String,
    pub resolv: String,
    /// systemd-resolved's list of upstream servers, used when `resolv` is its local stub.
    pub resolved: String,
    pub hostid: String,
}

impl Sources {
    pub(crate) fn running(paths: &Paths) -> Sources {
        let p = |x: &str| s(&paths.system(x));
        Sources {
            home: p("/home"),
            fspec: p("/opt/fspec"),
            esp: p("/efi"),
            shared_boot: p(SHARED_BOOT),
            resolv: p("/etc/resolv.conf"),
            resolved: p("/run/systemd/resolve/resolv.conf"),
            hostid: p("/etc/hostid"),
        }
    }
}

/// Create `distro`'s root on the layout and mount it at `root`. `top` is where the btrfs top
/// level is mounted.
pub(crate) fn create_root(plan: &mut Plan, layout: &Layout, distro: &str, top: &str, root: &str) {
    match layout {
        Layout::Zfs { pool, .. } => {
            let ds = format!("{pool}/ROOT/{distro}");
            plan.expect(
                Cmd::new("zfs").args(["list", "-H", "-o", "name", &ds]),
                false,
                format!("the dataset {ds} already exists; run `fspec forget {distro}` first"),
            );
            plan.run(Cmd::new("zfs").args(["create", "-o", "canmount=noauto", "-o", "mountpoint=/", &ds]));
            plan.run(Cmd::new("mkdir").arg("-p").arg(root));
            plan.run(Cmd::new("mount").args(["-t", "zfs", "-o", "zfsutil", &ds]).arg(root));
        }
        _ => {
            let new = subvol(distro);
            let uuid = layout.btrfs().map(|b| format!("UUID={}", b.uuid)).unwrap_or_default();
            plan.expect(
                Cmd::new("test").arg("-e").arg(format!("{top}/{new}")),
                false,
                format!("the subvolume {new} already exists; run `fspec forget {distro}` first"),
            );
            plan.run(Cmd::new("btrfs").args(["subvolume", "create"]).arg(format!("{top}/{new}")));
            plan.run(Cmd::new("mkdir").arg("-p").arg(root));
            plan.run(Cmd::new("mount").arg("-o").arg(format!("subvol={new}")).arg(&uuid).arg(root));
        }
    }
    plan.cleanup.insert(0, Cmd::new("umount").arg("-R").arg(root));
}

fn write(plan: &mut Plan, path: &str, text: String, exec: bool) {
    plan.run(Cmd::new("mkdir").arg("-p").arg(Path::new(path).parent().map(s).unwrap_or_default()));
    plan.run(Cmd::new("tee").arg(path).stdin(text));
    if exec {
        plan.run(Cmd::new("chmod").arg("755").arg(path));
    }
}

/// `cmd` run inside `root` with chroot.
fn in_root(root: &str, cmd: Cmd) -> Cmd {
    Cmd::new("chroot")
        .arg(root)
        .arg(&cmd.program)
        .args(&cmd.args)
        .env("HOME", "/root")
        // The running distro's PATH can miss the new root's tool dirs: Arch has no /usr/sbin.
        .env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
}

/// Bootstrap `t.distro` into the mounted `root`, set up its boot files for the layout, apply the
/// spec in it and build its kernels where the layout boots them.
pub(crate) fn install_root(
    plan: &mut Plan,
    layout: &Layout,
    t: &Target,
    root: &str,
    src: &Sources,
    cmdline: &str,
) -> Result<()> {
    let Some(boot) = Boot::of(layout) else { bail!("the disk is not on a supported layout") };
    let luks = layout.btrfs().is_some_and(|b| b.luks.is_some());
    let bootstrap = bootstrap(t, boot, luks, root, &src.esp)?;
    let d = t.distro;
    let options = kernel_options(layout, d, cmdline);
    let shared = format!("{}/{d}", src.shared_boot);

    if boot == Boot::Grub {
        // The kernel packages install into /boot, which is this root's folder of the shared partition.
        plan.run(Cmd::new("mkdir").arg("-p").arg(&shared).arg(format!("{root}/boot")));
        plan.run(Cmd::new("mount").arg("--bind").arg(&shared).arg(format!("{root}/boot")));
    }
    // moss syncs boot while it installs, into the ESP it finds under the root and with the kernel
    // options already there.
    let moss_boots = d == "aerynos";
    if moss_boots {
        bind(plan, &src.esp, &format!("{root}/efi"));
        write(plan, &format!("{root}{}", cmdline_file(d)), format!("{options}\n"), false);
    }
    bootstrap.into_iter().for_each(|c| plan.run(c));
    let resolv = r#"src=$1; stub=; if [ -r "$src" ]; then while read -r k v _; do [ "$k" = nameserver ] && [ "$v" = 127.0.0.53 ] && stub=1; done <"$src"; fi; if [ -n "$stub" ] && [ -s "$2" ]; then src=$2; fi; rm -f "$3"; cp -L "$src" "$3""#;
    plan.run(
        Cmd::new("sh").args(["-c", resolv, "sh", &src.resolv, &src.resolved]).arg(format!("{root}/etc/resolv.conf")),
    );
    write(plan, &format!("{root}/etc/fstab"), fstab(layout, d), false);
    if let Some(luks) = layout.btrfs().and_then(|b| b.luks.as_ref()) {
        let opts = if matches!(d, "debian" | "ubuntu") { "luks,discard,initramfs" } else { "luks,discard" };
        write(plan, &format!("{root}/etc/crypttab"), format!("luks-{luks} UUID={luks} none {opts}\n"), false);
    }
    match layout {
        Layout::Zfs { pool, .. } => {
            plan.run(Cmd::new("cp").arg(&src.hostid).arg(format!("{root}/etc/hostid")));
            plan.run(
                Cmd::new("zfs")
                    .arg("set")
                    .arg(format!("org.zfsbootmenu:commandline={options}"))
                    .arg(format!("{pool}/ROOT/{d}")),
            );
        }
        _ if moss_boots => {}
        _ => write(plan, &format!("{root}{}", cmdline_file(d)), format!("{options}\n"), false),
    }
    if boot == Boot::Grub {
        write(plan, &format!("{shared}/fspec-cmdline"), format!("{options}\n"), false);
    }
    for f in boot_files(d, boot, luks) {
        write(plan, &format!("{root}{}", f.path), f.text, f.exec);
    }

    // The shared home goes in before the apply, so a declared user's home lands in it.
    let mut binds = vec![(src.home.as_str(), "home"), (src.fspec.as_str(), "opt/fspec")];
    if boot != Boot::Grub && !moss_boots {
        binds.push((src.esp.as_str(), "efi"));
    }
    for (from, to) in binds.into_iter().chain([("/dev", "dev"), ("/sys", "sys")]) {
        bind(plan, from, &format!("{root}/{to}"));
    }
    if moss_boots {
        // Inside the root, blsforme takes the ESP the running loader names in its EFI variables,
        // which from an installer is the installer's own. Without them it takes the ESP on the
        // root's disk.
        let hide = r#"if [ -d "$1" ]; then mount -t tmpfs fspec-no-efivars "$1"; fi"#;
        plan.run(Cmd::new("sh").args(["-c", hide, "sh"]).arg(format!("{root}/sys/firmware/efi/efivars")));
    }
    // moss leaves the root without the mount point.
    plan.run(Cmd::new("mkdir").arg("-p").arg(format!("{root}/proc")));
    plan.run(Cmd::new("mount").args(["-t", "proc", "proc"]).arg(format!("{root}/proc")));

    let spec_copy = format!("{root}/var/lib/fspec/spec");
    plan.run(Cmd::new("mkdir").arg("-p").arg(&spec_copy));
    // Moved into place from inside the root: AerynOS links /usr/local to /var/local with an
    // absolute link, which a path from out here would follow onto the running system.
    plan.run(Cmd::new("cp").arg(s(t.exe)).arg(format!("{root}/fspec.new")));
    let place = r#"mkdir -p "$(readlink -f /usr/local)/bin" && mv /fspec.new /usr/local/bin/fspec"#;
    plan.run(in_root(root, Cmd::new("sh").args(["-c", place])));
    plan.run(Cmd::new("cp").arg("-a").arg(format!("{}/.", s(t.spec_dir))).arg(&spec_copy));
    let apply = Cmd::new("/usr/local/bin/fspec")
        .arg("--spec")
        .arg(format!("/var/lib/fspec/spec/{}", t.spec_file))
        .args(["apply", "--local", "--scope", "system", "--host", t.host]);
    if let Some(sh) = upgrade(d, t.arch, boot, luks) {
        plan.run(in_root(root, Cmd::new("sh").args(["-c", &sh])));
    }
    plan.run(in_root(root, apply));
    plan.run(in_root(root, Cmd::new("sh").args(["-c", &populate(d, boot, luks)])));
    Ok(())
}

fn bind(plan: &mut Plan, from: &str, to: &str) {
    plan.run(Cmd::new("mkdir").arg("-p").arg(to));
    plan.run(Cmd::new("mount").arg("--rbind").arg(from).arg(to));
    plan.run(Cmd::new("mount").arg("--make-rslave").arg(to));
}

/// Refuse to touch boot from a root that does not own it, so the bootloader's default never goes
/// with a deleted root.
pub(crate) fn require_owner(m: &Machine, owner: Option<&str>) -> Result<()> {
    match owner {
        Some(o) if o == m.running => Ok(()),
        Some(o) => bail!(
            "boot is owned by {o}, not this root ({}); run `fspec switch --adopt-boot` from {} first",
            m.running,
            m.running
        ),
        None => bail!("no root is recorded as owning boot; run `fspec switch --adopt-boot` from {} first", m.running),
    }
}

/// Install `t.distro` into a new root next to the running one, apply the spec in it and make it
/// bootable. The old root, and `/home`, are untouched.
pub fn switch_distro(paths: &Paths, m: &Machine, owner: Option<&str>, t: &Target) -> Result<Plan> {
    if let Some(why) = misfit(recipe(t.distro)?, &m.layout, &m.firmware) {
        bail!("cannot switch to {}: {why}", t.distro);
    }
    let mut plan = Plan::default();
    let Mounts { top, root } = mounts(paths, t.distro);
    if let Some(b) = m.layout.btrfs() {
        plan.run(Cmd::new("mkdir").arg("-p").arg(&top));
        plan.run(
            Cmd::new("mount")
                .arg("-o")
                .arg(format!("subvolid=5,{BTRFS_OPTS}"))
                .arg(format!("UUID={}", b.uuid))
                .arg(&top),
        );
        plan.cleanup.push(Cmd::new("umount").arg(&top));
    }
    create_root(&mut plan, &m.layout, t.distro, &top, &root);
    let src = Sources::running(paths);
    install_root(&mut plan, &m.layout, t, &root, &src, &m.cmdline)?;

    // Until a root adopts boot, the running one stays the default.
    match &m.layout {
        Layout::Default { .. } if owner.is_none() => {
            sd_default(&s(&paths.system(m.layout.loader_dir())), &m.running).into_iter().for_each(|c| plan.run(c))
        }
        Layout::Zfs { pool, .. } if owner.is_none() => {
            plan.run(Cmd::new("zpool").arg("set").arg(format!("bootfs={}", m.root)).arg(pool))
        }
        Layout::Bios { boot, .. } => plan.run(grub_menu(&src.shared_boot, &boot.uuid, owner.unwrap_or(&m.running))),
        _ => {}
    }
    Ok(plan.private(&s(&paths.system(WORK))))
}
