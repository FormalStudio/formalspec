use std::path::Path;

use anyhow::{Result, bail};
use fspec_core::Paths;
use fspec_core::runner::Cmd;
use fspec_core::zypp::{SNAPSHOT_REPOS, SNAPSHOT_VAR, snapshot_repo};

use crate::beside::{
    BTRFS_OPTS, DEADLINE_SERVICE, INHERIT, WATCHDOG, deadline_timer, delete_new, esp, guard_service, valid_name,
};
use crate::classic::{WORK, s};
use crate::machine::{ESP_TYPE, Firmware, Machine};
use crate::{Plan, subvol};

/// The official Tumbleweed container zypper runs from. Not pinned by digest: the registry drops
/// old digests within weeks, and the packages come from the locked snapshot's repos, not from it.
pub const TUMBLEWEED_IMAGE: &str = "registry.opensuse.org/opensuse/tumbleweed:latest";

/// What the root gets beyond the minimal base pattern. Recommends stay out, so firmware comes
/// from the metapackage rather than from what zypper would match against this machine.
const PACKAGES: [&str; 22] = [
    "patterns-base-minimal_base",
    // The pattern leaves zypper itself to recommends.
    "zypper",
    "ca-certificates-mozilla",
    "kernel-default",
    "kernel-firmware-all",
    "dracut",
    "btrfsprogs",
    "NetworkManager",
    "openssh-server",
    "openssh-clients",
    "sudo",
    "iproute2",
    "snapper",
    "snapper-zypp-plugin",
    "zram-generator",
    "tailscale",
    "power-profiles-daemon",
    "system-group-wheel",
    "bash",
    // `fspec apply` from another host ships the spec as a tarball.
    "tar",
    // A spec's /etc/localtime link points into it before any package is applied.
    "timezone",
    // `git(...)` sources, built here or for another host, are checked out with it.
    "git-core",
];

/// Runs in the Tumbleweed container with the new root at `/mnt`: `$1` is the snapshot, `$2` the
/// zypp variable that holds it, the rest the packages. The kernel's scriptlets build the initrd, so the root gets the API filesystems.
const BOOTSTRAP: &str = r#"set -eu
snapshot=$1 var=$2
shift 2
for d in dev proc sys run; do mkdir -p "/mnt/$d"; done
mount --rbind /dev /mnt/dev
mount -t proc proc /mnt/proc
mount -t sysfs sysfs /mnt/sys
mount -t tmpfs tmpfs /mnt/run
printf '%s\n' "$snapshot" >"/mnt/etc/zypp/vars.d/$var"
# Idle priority, so the machine stays usable while it downloads and unpacks.
nice -n 19 ionice -c3 zypper --root /mnt --gpg-auto-import-keys -n refresh
nice -n 19 ionice -c3 zypper --root /mnt -n install --no-recommends "$@"
"#;

/// Runs chrooted in the new root, from the Tumbleweed container, once fspec's files are in.
const CONFIGURE: &str = r#"set -eu
systemctl enable NetworkManager.service sshd.service tailscaled.service fspec-esp-sync.service \
	fspec-esp-sync.path fspec-boot-guard.service fspec-boot-deadline.timer fspec-power-saver.service
systemctl set-default multi-user.target
# snapper reads the mounts from /etc/mtab, which a booted system gets from tmpfiles.
[ -e /etc/mtab ] || ln -s ../proc/self/mounts /etc/mtab
# The snapshots live in a subvolume nested in the root, so they go wherever the root goes and
# the other roots never see them at the top level.
[ -d /.snapshots ] || snapper --no-dbus -c root create-config /
for m in /usr/lib/modules/*/; do
	k=${m%/}
	k=${k##*/}
	[ -f "$m/vmlinuz" ] || continue
	[ -s "/boot/initrd-$k" ] || dracut --force --kver "$k" "/boot/initrd-$k"
done
"#;

/// Copies the kernels and initrds of the root at `$1` to the ESP at `$2`, under `$2/$3/<kernel>/`,
/// with a boot entry `$3-<kernel>.conf` each that counts three tries when it is new. While an entry
/// still counts tries it masks the emergency and rescue targets, so a trial boot that fails a mount
/// goes on to sshd degraded instead of waiting offline at a shell; a blessed entry gets the shell
/// back for hands-on recovery. Touches only `$2/$3/` and `$2/loader/entries/$3-*.conf`, and
/// `$2/EFI/limine/limine.conf` when the root has `/etc/fspec/limine.conf`: that file's menu,
/// then a `Kernels` directory with every boot entry on the ESP, so Limine boots any kernel
/// straight as well as through systemd-boot. Limine counts no tries, so those entries go
/// around the boot guard; the menu's default entry is the one that should chainload systemd-boot.
pub const ESP_SYNC: &str = r#"#!/bin/sh
set -eu
root=$1 esp=$2 name=$3
dir=$esp/$name
entries=$esp/loader/entries
if [ ! -d "$entries" ]; then
	echo "fspec-esp-sync: $entries is missing" >&2
	exit 1
fi
options=$(cat "$root/etc/kernel/cmdline")
trial="systemd.mask=emergency.target systemd.mask=emergency.service systemd.mask=rescue.target systemd.mask=rescue.service"
title=$(sed -n 's/^PRETTY_NAME="\{0,1\}\([^"]*\)"\{0,1\}$/\1/p' "$root/usr/lib/os-release")
mkdir -p "$dir"
limine=$esp/EFI/limine/limine.conf
trap 'rm -f "$dir"/*/*.new "$entries/$name"-*.new "$limine.new"' EXIT
kernels=" "
for m in "$root"/usr/lib/modules/*/; do
	k=${m%/}
	k=${k##*/}
	initrd=$root/boot/initrd-$k
	[ -f "$m/vmlinuz" ] && [ -s "$initrd" ] || continue
	kernels="$kernels$k "
	mkdir -p "$dir/$k"
	for f in "linux $m/vmlinuz" "initrd $initrd"; do
		to=$dir/$k/${f%% *}
		from=${f#* }
		if [ -f "$to" ] && ! [ "$from" -nt "$to" ] && [ "$(wc -c <"$from")" = "$(wc -c <"$to")" ]; then
			continue
		fi
		cp "$from" "$to.new"
		touch -r "$from" "$to.new"
		mv "$to.new" "$to"
	done
	entry=$entries/$name-$k+3.conf
	for f in "$entries/$name-$k.conf" "$entries/$name-$k"+*.conf; do
		if [ -e "$f" ]; then entry=$f; break; fi
	done
	case ${entry##*/} in
	*+*) opts="$options $trial" ;;
	*) opts=$options ;;
	esac
	printf 'title %s (%s)\nversion %s\nlinux /%s/%s/linux\ninitrd /%s/%s/initrd\noptions %s\n' \
		"$title" "$k" "$k" "$name" "$k" "$name" "$k" "$opts" >"$entry.new"
	if [ -f "$entry" ] && [ "$(cat "$entry")" = "$(cat "$entry.new")" ]; then
		rm "$entry.new"
	else
		mv "$entry.new" "$entry"
	fi
done
for f in "$entries/$name"-*.conf; do
	[ -e "$f" ] || continue
	k=${f##*/}
	k=${k%.conf}
	k=${k%%+*}
	k=${k#"$name"-}
	case $kernels in *" $k "*) ;; *) rm -f "$f" ;; esac
done
for d in "$dir"/*/; do
	[ -d "$d" ] || continue
	k=${d%/}
	k=${k##*/}
	case $kernels in *" $k "*) ;; *) rm -rf "$d" ;; esac
done
if [ -f "$root/etc/fspec/limine.conf" ] && [ -d "${limine%/*}" ]; then
	{
		cat "$root/etc/fspec/limine.conf"
		printf '\n/Kernels\n    comment: Every kernel on this disk, booted straight, without counting tries\n'
		for f in "$entries"/*.conf; do
			[ -e "$f" ] || continue
			awk '
				/^title / { sub(/^title +/, ""); t = $0 }
				/^version / { sub(/^version +/, ""); v = $0 }
				/^linux / { l = $2 }
				/^initrd / { i = i "    module_path: boot():" $2 "\n" }
				/^options / { sub(/^options +/, ""); o = $0 }
				END {
					if (l == "") exit
					if (v != "" && index(t, v) == 0) t = t " (" v ")"
					printf "//%s\n    protocol: linux\n    path: boot():%s\n%s    cmdline: %s\n", t, l, i, o
				}' "$f"
		done
	} >"$limine.new"
	if cmp -s "$limine" "$limine.new"; then rm "$limine.new"; else mv "$limine.new" "$limine"; fi
fi
sync -f "$entries"
"#;

/// Reruns the sync when a spec writes Limine's menu header, whichever resource writes it.
pub const ESP_SYNC_PATH: &str = "[Unit]
Description=Rewrite Limine's menu when its header changes (fspec)

[Path]
PathChanged=/etc/fspec/limine.conf
Unit=fspec-esp-sync.service

[Install]
WantedBy=multi-user.target
";

/// libzypp commit plugin. COMMITEND comes after the transaction's scriptlets, so the initrds are
/// rebuilt by then. Every frame ends in a NUL byte and gets an ACK; the plugin's stdout is the
/// protocol, so what the sync prints goes to stderr.
pub(crate) fn zypp_plugin(name: &str) -> String {
    format!(
        r#"#!/bin/bash
while IFS= read -r -d '' frame; do
	frame=${{frame#$'\n'}}
	command=${{frame%%$'\n'*}}
	if [ "$command" = COMMITEND ]; then
		/bin/sh /etc/fspec/esp-sync.sh / /boot/efi {name} >&2 || echo "fspec: copying the kernels to the ESP failed" >&2
	fi
	printf 'ACK\n\n\0'
	[ "$command" = _DISCONNECT ] && exit 0
done
"#
    )
}

/// Passes once sshd listens and, when the inherited root had tailscale state, tailscale is
/// online; fails after five minutes otherwise, so a boot nobody can reach is never blessed.
/// tailscaled writes a state file of its own before anyone logs in, so the install leaves a marker.
const GUARD: &str = r#"#!/bin/sh
end=$(($(date +%s) + 300))
up() {
	systemctl is-active -q sshd.service && ss -Htlnp | grep -q '"sshd"' || return 1
	[ -e /etc/fspec/needs-tailscale ] || return 0
	tailscale status --json --peers=false 2>/dev/null | grep -q '"Online": true'
}
until up; do
	if [ "$(date +%s)" -ge "$end" ]; then
		echo "sshd or tailscale is not up after five minutes; leaving this boot unblessed" >&2
		exit 1
	fi
	sleep 5
done
"#;

/// Writes the kernel options after `$3` to the file `$3`, with the `console=` options of the
/// entries on the ESP at `$1` that boot the subvolume `$2`, since a live system's own console says
/// nothing about the machine.
const CMDLINE: &str = r#"set -eu
esp=$1 from=$2 out=$3
shift 3
console=$(grep -h '^options' "$esp"/loader/entries/*.conf 2>/dev/null | grep -F "subvol=$from" |
	tr ' ' '\n' | grep '^console=' | awk '!seen[$0]++' | tr '\n' ' ')
mkdir -p "${out%/*}"
printf '%s %s\n' "$*" "$console" | sed 's/ *$//' >"$out"
"#;

/// Runs [`INHERIT`], given as `$1`, with the fstab line `$3` for the shared home only when the
/// top level `$2` has `@home`.
const WITH_HOME: &str = r#"set -eu
script=$1 top=$2 home=$3 src=$4 root=$5 etc=$6 var=$7
shift 7
[ -d "$top/@home" ] || home=
exec sh -c "$script" sh "$src" "$root" "$etc" "$var" "$home" "$@"
"#;

fn sync_service(name: &str) -> String {
    format!(
        "[Unit]
Description=Copy this root's kernels and boot entries to the shared ESP (fspec)
RequiresMountsFor=/boot/efi
After=local-fs.target

[Service]
Type=oneshot
ExecStart=/bin/sh /etc/fspec/esp-sync.sh / /boot/efi {name}

[Install]
WantedBy=multi-user.target
"
    )
}

/// Picks the power-saver profile on the first boot. power-profiles-daemon keeps the profile set
/// last, so a later choice sticks. The D-Bus call starts the daemon, which is ordered after
/// `multi-user.target`; without default dependencies the target is not ordered after this unit.
const POWER_SAVER: &str = "[Unit]
Description=Start this root in the power-saver profile (fspec)
ConditionPathExists=!/var/lib/fspec/power-saver
DefaultDependencies=no
After=multi-user.target dbus.service
Conflicts=shutdown.target
Before=shutdown.target

[Service]
Type=oneshot
ExecStart=/usr/bin/busctl set-property org.freedesktop.UPower.PowerProfiles /org/freedesktop/UPower/PowerProfiles \
org.freedesktop.UPower.PowerProfiles ActiveProfile s power-saver
ExecStart=/bin/sh -c 'mkdir -p /var/lib/fspec && touch /var/lib/fspec/power-saver'

[Install]
WantedBy=multi-user.target
";

/// Rewrites the booted entry right after `systemd-bless-boot` dropped its counter, which takes
/// the trial options out of it. A drop-in, since a unit wanted by `boot-complete.target` would be
/// ordered both before and after it.
fn bless_dropin(name: &str) -> String {
    format!(
        "[Unit]
RequiresMountsFor=/boot/efi

[Service]
ExecStartPost=-/bin/sh /etc/fspec/esp-sync.sh / /boot/efi {name}
"
    )
}

/// What `switch --beside <name>` installs for a `tumbleweed()` host.
pub struct Tumbleweed<'a> {
    /// Names the subvolume `@<name>`, the ESP directory and the entries.
    pub name: &'a str,
    /// The `YYYYMMDD` history snapshot `fspec.lock` pins the host's repos to.
    pub snapshot: &'a str,
    /// The subvolume whose identity the root keeps.
    pub inherit: &'a str,
    pub users: Vec<String>,
    /// The host's `tumbleweed(kernel_args = ...)`, after the options the root needs to boot.
    pub kernel_args: &'a [String],
}

/// What a `tumbleweed()` host running under `fspec vm` gets on its first boot, so it boots the way
/// a root `switch --beside` installed does: systemd-boot on the ESP, ahead of the cloud image's
/// shim and grub in the firmware's order, and fspec-esp-sync copying this root's kernels there
/// with entries that count tries. The files go in first, then the script runs as root.
pub fn vm_boot(name: &str, kernel_args: &[String]) -> (Vec<(String, String, bool)>, String) {
    let files = [
        ("etc/fspec/esp-sync.sh", ESP_SYNC.to_string(), false),
        ("usr/lib/zypp/plugins/commit/fspec-esp-sync", zypp_plugin(name), true),
        ("etc/systemd/system/fspec-esp-sync.service", sync_service(name), false),
        ("etc/systemd/system/fspec-esp-sync.path", ESP_SYNC_PATH.to_string(), false),
        ("etc/systemd/system/systemd-bless-boot.service.d/fspec.conf", bless_dropin(name), false),
    ]
    .map(|(p, text, exec)| (format!("/{p}"), text, exec))
    .to_vec();
    let args: String = kernel_args.iter().map(|a| format!("{a}\n")).collect();
    let script = format!(
        r#"set -eu
zypper -n --gpg-auto-import-keys install --no-recommends systemd-boot
bootctl install
# The image's console and LSM options carry over, since its root is labeled for its policy; the
# rest is what a beside root boots with.
image=$(tr ' ' '\n' </proc/cmdline | grep -E '^(console|security|selinux|enforcing)=' || true)
# A serial console makes plymouth force its text splash; a real host has none.
case $image in *console=ttyS*) image="$image plymouth.ignore-serial-consoles" ;; esac
args=$(cat <<'FSPEC_ARGS'
{args}FSPEC_ARGS
)
printf '%s\n' "root=UUID=$(findmnt -no UUID /)" rw panic=10 rd.shell=0 rd.emergency=reboot $image $args |
	awk 'NF && !seen[$0]++' | paste -sd ' ' - >/etc/kernel/cmdline
systemctl daemon-reload
systemctl enable fspec-esp-sync.service fspec-esp-sync.path
sh /etc/fspec/esp-sync.sh / /boot/efi {name}
"#
    );
    (files, script)
}

/// Bootstrap openSUSE Tumbleweed into a new subvolume `@<name>` beside the other roots of the btrfs
/// filesystem of `m`, from a live system or from one of those roots. zypper runs from the official
/// container against the mounted subvolume, so Tumbleweed's own rpm writes the rpmdb. The root
/// keeps the identity of `t.inherit`, and its kernels go onto the ESP with entries that count
/// three tries; a zypp plugin and a boot-time unit keep them current. `loader.conf`, the other
/// roots' entries and the EFI variables stay as they are.
pub fn tumbleweed(paths: &Paths, m: &Machine, t: &Tumbleweed) -> Result<Plan> {
    let name = t.name;
    valid_name(name)?;
    if m.firmware != Firmware::Efi {
        bail!("this machine booted through BIOS; --beside boots the new root through systemd-boot on the ESP");
    }
    if m.fstype != "btrfs" {
        bail!("the filesystem is {}; --beside puts the new root in a btrfs subvolume", m.fstype);
    }
    if t.snapshot.len() != 8 || !t.snapshot.chars().all(|c| c.is_ascii_digit()) {
        bail!("--snapshot takes a Tumbleweed snapshot date like 20261020, not `{}`", t.snapshot);
    }
    let from = t.inherit;
    if !from.starts_with('@') || from.contains('/') || from.contains("..") {
        bail!("--inherit takes a top-level subvolume like @nixos, not `{from}`");
    }
    let work = paths.system(WORK);
    let mut plan = Plan::default();
    let esp_dir = match esp(m) {
        Some((_, at)) => s(&paths.system(at)),
        None => {
            let mut parts = m.parts.iter().filter(|p| p.parttype == ESP_TYPE);
            let (Some(part), None) = (parts.next(), parts.next()) else {
                bail!("{} needs exactly one EFI system partition", m.disk)
            };
            let at = s(&work.join("esp"));
            plan.run(Cmd::new("mkdir").arg("-p").arg(&at));
            plan.run(Cmd::new("mount").arg(&part.dev).arg(&at));
            plan.cleanup.push(Cmd::new("umount").arg(&at));
            at
        }
    };
    let esp_part = m.parts.iter().find(|p| p.parttype == ESP_TYPE).expect("found above");
    let new = subvol(name);
    let (top, root) = (s(&work.join("top")), s(&work.join(name)));
    let source = format!("{top}/{from}");
    let uuid = &m.fs_uuid;

    plan.expect(
        Cmd::new("test").arg("-e").arg(format!("{esp_dir}/EFI/systemd/systemd-bootx64.efi")),
        true,
        format!("systemd-boot is not installed on the ESP at {esp_dir}"),
    );
    plan.run(Cmd::new("mkdir").arg("-p").arg(&top));
    plan.run(Cmd::new("mount").args(["-o", "subvolid=5"]).arg(format!("UUID={uuid}")).arg(&top));
    plan.cleanup.push(Cmd::new("umount").arg(&top));
    plan.expect(
        Cmd::new("test").arg("-e").arg(format!("{top}/{new}")),
        false,
        format!("the subvolume {new} already exists; delete it first"),
    );
    plan.expect(
        Cmd::new("test").arg("-f").arg(format!("{source}/etc/passwd")),
        true,
        format!("{source} holds no root to inherit from"),
    );
    for u in &t.users {
        plan.expect(
            Cmd::new("grep").args(["-q", &format!("^{u}:")]).arg(format!("{source}/etc/passwd")),
            true,
            format!("the spec's user {u} does not exist in {source}, the root this one inherits its users from"),
        );
    }
    plan.run(Cmd::new("btrfs").args(["subvolume", "create"]).arg(format!("{top}/{new}")));
    // A failed install leaves no half-built root behind, which would block the next try. The
    // undo is reached only once this run created the subvolume, and runs after the cleanup has
    // unmounted the top level, so it mounts it again.
    plan.undo(delete_new(uuid, &new));
    plan.run(Cmd::new("mkdir").arg("-p").arg(&root));
    plan.run(
        Cmd::new("mount").arg("-o").arg(format!("subvol={new},{BTRFS_OPTS}")).arg(format!("UUID={uuid}")).arg(&root),
    );
    plan.cleanup.insert(0, Cmd::new("umount").arg("-R").arg(&root));

    let at = |p: &str| format!("{root}/{p}");
    let write = |plan: &mut Plan, path: &str, text: String| {
        let path = at(path);
        plan.run(Cmd::new("mkdir").arg("-p").arg(Path::new(&path).parent().map(s).unwrap_or_default()));
        plan.run(Cmd::new("tee").arg(path).stdin(text));
    };

    // Before the kernel is installed, so its scriptlets build the initrd this root boots with.
    let mut options = vec![format!("root=UUID={uuid}"), format!("rootflags=subvol={new},{BTRFS_OPTS}"), "rw".into()];
    if let Some(luks) = &m.luks {
        options.push(format!("rd.luks.uuid={luks}"));
    }
    // A boot that fails in the initrd reboots instead of waiting at a shell, so the next try or the
    // next entry gets its turn.
    options.extend(["panic=10", "rd.shell=0", "rd.emergency=reboot"].map(String::from));
    options.extend(t.kernel_args.iter().cloned());
    plan.run(Cmd::new("sh").args(["-c", CMDLINE, "sh", &esp_dir, from, &at("etc/kernel/cmdline")]).args(&options));
    write(
        &mut plan,
        "etc/dracut.conf.d/50-fspec.conf",
        // Built in a container, host-only detection would describe the wrong machine.
        "hostonly=\"no\"\nadd_dracutmodules+=\" btrfs \"\n".into(),
    );
    // The repos `fspec apply` keeps at the locked snapshot, so the root starts without drift.
    for (alias, path) in SNAPSHOT_REPOS {
        write(&mut plan, &format!("etc/zypp/repos.d/{alias}.repo"), snapshot_repo(alias, path));
    }
    plan.run(Cmd::new("mkdir").arg("-p").arg(at("etc/zypp/vars.d")));
    plan.run(
        Cmd::new("podman")
            .args(["run", "--rm", "--privileged", "--network", "host", "-v"])
            .arg(format!("{root}:/mnt"))
            .args([TUMBLEWEED_IMAGE, "sh", "-c", BOOTSTRAP, "sh", t.snapshot, SNAPSHOT_VAR])
            .args(PACKAGES),
    );

    let mut fstab = format!("UUID={uuid} / btrfs subvol={new},{BTRFS_OPTS} 0 0\n");
    let what = if esp_part.partuuid.is_empty() {
        format!("UUID={}", esp_part.uuid)
    } else {
        format!("PARTUUID={}", esp_part.partuuid)
    };
    fstab.push_str(&format!("{what} /boot/efi vfat umask=0077,shortname=winnt,nofail 0 2\n"));
    let files = [
        ("etc/fstab", fstab, false),
        ("etc/fspec/esp-sync.sh", ESP_SYNC.to_string(), false),
        ("etc/fspec/boot-guard.sh", GUARD.to_string(), false),
        ("usr/lib/zypp/plugins/commit/fspec-esp-sync", zypp_plugin(name), true),
        ("etc/systemd/system/fspec-esp-sync.service", sync_service(name), false),
        ("etc/systemd/system/fspec-esp-sync.path", ESP_SYNC_PATH.to_string(), false),
        ("etc/systemd/system/systemd-bless-boot.service.d/fspec.conf", bless_dropin(name), false),
        ("etc/systemd/system/fspec-power-saver.service", POWER_SAVER.to_string(), false),
        ("etc/systemd/system/fspec-boot-guard.service", guard_service(), false),
        ("etc/systemd/system/fspec-boot-deadline.timer", deadline_timer(), false),
        ("etc/systemd/system/fspec-boot-deadline.service", DEADLINE_SERVICE.to_string(), false),
        ("etc/systemd/system.conf.d/fspec-watchdog.conf", WATCHDOG.to_string(), false),
        // Not NixOS's swapfile: a Tumbleweed boot would overwrite a hibernated NixOS's image.
        ("etc/systemd/zram-generator.conf", "[zram0]\nzram-size = min(ram / 2, 4096)\n".into(), false),
    ];
    for (path, text, exec) in files {
        write(&mut plan, path, text);
        if exec {
            plan.run(Cmd::new("chmod").arg("755").arg(at(path)));
        }
    }
    plan.run(Cmd::new("mkdir").arg("-p").arg(at("boot/efi")));
    let home = format!("UUID={uuid} /home btrfs subvol=@home,{BTRFS_OPTS} 0 0");
    plan.run(Cmd::new("sh").args([
        "-c",
        "if [ -d \"$1/var/lib/tailscale\" ]; then : >\"$2\"; fi",
        "sh",
        &source,
        &at("etc/fspec/needs-tailscale"),
    ]));
    plan.run(
        Cmd::new("sh")
            .args(["-c", WITH_HOME, "sh", INHERIT, &top, &home, &source, &root, &at("etc"), &at("var")])
            .args(t.users.iter().map(String::as_str)),
    );
    plan.run(
        Cmd::new("podman")
            .args(["run", "--rm", "--privileged", "--network", "none", "-v"])
            .arg(format!("{root}:/mnt"))
            .args([TUMBLEWEED_IMAGE, "sh", "-c"])
            .arg(
                "set -eu; mount --rbind /dev /mnt/dev; mount -t proc proc /mnt/proc; mount -t sysfs sysfs /mnt/sys; \
                 mount -t tmpfs tmpfs /mnt/run; exec chroot /mnt sh -c \"$1\"",
            )
            .args(["sh", CONFIGURE]),
    );
    plan.run(Cmd::new("sh").arg(at("etc/fspec/esp-sync.sh")).arg(&root).arg(&esp_dir).arg(name));
    Ok(plan.private(&s(&work)))
}
