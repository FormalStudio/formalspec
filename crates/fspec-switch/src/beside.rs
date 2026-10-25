use std::path::Path;

use anyhow::{Result, bail};
use fspec_core::Paths;
use fspec_core::runner::Cmd;

use crate::classic::{WORK, s};
use crate::machine::{ESP_TYPE, Firmware, Machine, Part};
use crate::{Plan, subvol};

/// Copies the kernels and boot entries of the ostree deployments under the sysroot `$1` to the ESP
/// at `$2`, since systemd-boot cannot read them from btrfs. It touches only `$2/$3/` and
/// `$2/loader/entries/$3-ostree-*.conf`.
pub(crate) const ESP_SYNC: &str = r#"#!/bin/sh
set -eu
sysroot=$1 esp=$2 distro=$3
src=$sysroot/boot/loader/entries
dir=$esp/$distro
entries=$esp/loader/entries
# A number per deployment, given once and never reused. Entries without a sort-key sort by name,
# highest first, so the newest deployment leads; the name stays put, so systemd-boot's boot
# counter in it does too.
ids=$dir/ids
if [ ! -d "$src" ] || [ ! -d "$entries" ]; then
	echo "fspec-esp-sync: $src or $entries is missing" >&2
	exit 1
fi
mkdir -p "$dir"
touch "$ids"
trap 'rm -f "$dir"/*/*.new "$entries/$distro"-ostree-*.new' EXIT

# ostree writes the paths relative to /boot when /boot is its own partition.
file() {
	case $1 in
	/boot/*) echo "$sysroot$1" ;;
	*) echo "$sysroot/boot$1" ;;
	esac
}

keep=" " kernels=" "
for conf in "$src"/*.conf; do
	[ -e "$conf" ] || continue
	title= options= linux= initrd=
	while read -r key value; do
		case $key in
		title) title=$value ;;
		options) options=$value ;;
		linux) linux=$value ;;
		initrd) initrd=$value ;;
		esac
	done <"$conf"
	ostree=${options##*ostree=}
	ostree=${ostree%% *}
	deployment=$(readlink -f "$sysroot$ostree")
	deployment=${deployment##*/}
	n=
	while read -r d i; do
		if [ "$d" = "$deployment" ]; then n=$i; fi
	done <"$ids"
	if [ -z "$n" ]; then
		n=0
		while read -r d i; do
			if [ "$i" -gt "$n" ]; then n=$i; fi
		done <"$ids"
		n=$((n + 1))
		echo "$deployment $n" >>"$ids"
	fi
	kernel=${linux%/*}
	kernel=${kernel##*/}
	if [ ! -e "$dir/$kernel/initramfs.img" ]; then
		mkdir -p "$dir/$kernel"
		cp "$(file "$linux")" "$dir/$kernel/vmlinuz.new"
		mv "$dir/$kernel/vmlinuz.new" "$dir/$kernel/vmlinuz"
		cp "$(file "$initrd")" "$dir/$kernel/initramfs.img.new"
		mv "$dir/$kernel/initramfs.img.new" "$dir/$kernel/initramfs.img"
	fi
	name=$distro-ostree-$n
	entry=$entries/$name+3.conf
	for f in "$entries/$name.conf" "$entries/$name"+*.conf; do
		if [ -e "$f" ]; then entry=$f; fi
	done
	text=$(printf 'title %s\nlinux /%s/%s/vmlinuz\ninitrd /%s/%s/initramfs.img\noptions %s' \
		"$title" "$distro" "$kernel" "$distro" "$kernel" "$options")
	if [ ! -e "$entry" ] || [ "$(cat "$entry")" != "$text" ]; then
		printf '%s\n' "$text" >"$entries/$name.new"
		mv "$entries/$name.new" "$entry"
	fi
	keep="$keep$name " kernels="$kernels$kernel "
done
if [ "$keep" = " " ]; then
	echo "fspec-esp-sync: no ostree entries in $src" >&2
	exit 1
fi
for f in "$entries/$distro"-ostree-*.conf; do
	[ -e "$f" ] || continue
	name=${f##*/}
	name=${name%.conf}
	name=${name%%+*}
	case $keep in *" $name "*) ;; *) rm -f "$f" ;; esac
done
for d in "$dir"/*/; do
	[ -d "$d" ] || continue
	k=${d%/}
	k=${k##*/}
	case $kernels in *" $k "*) ;; *) rm -rf "$d" ;; esac
done
sync -f "$entries"
"#;

/// Passes once sshd listens and tailscale is online, and fails after five minutes otherwise, so
/// a boot nobody can reach is never blessed.
pub(crate) const GUARD: &str = r#"#!/bin/sh
end=$(($(date +%s) + 300))
until systemctl is-active -q sshd.service && ss -Htlnp | grep -q '"sshd"' &&
	tailscale status --json --peers=false 2>/dev/null | grep -q '"Online": true'; do
	if [ "$(date +%s)" -ge "$end" ]; then
		echo "sshd or tailscale is not up after five minutes; leaving this boot unblessed" >&2
		exit 1
	fi
	sleep 5
done
"#;

/// systemd-boot sets it when it boots an entry that still counts tries.
pub(crate) const COUNTING: &str = "/sys/firmware/efi/efivars/LoaderBootCountPath-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f";

pub(crate) fn esp_mount(esp: &Part) -> String {
    let what =
        if esp.partuuid.is_empty() { format!("UUID={}", esp.uuid) } else { format!("PARTUUID={}", esp.partuuid) };
    format!(
        "[Unit]
Description=EFI system partition shared with the other roots (fspec)

[Mount]
What={what}
Where=/boot/efi
Type=vfat
Options=umask=0077,shortname=winnt

[Install]
WantedBy=local-fs.target
"
    )
}

fn sync_service(distro: &str) -> String {
    format!(
        "[Unit]
Description=Copy this root's kernels and boot entries to the shared ESP (fspec)
DefaultDependencies=no
ConditionPathExists=/run/ostree-booted
RequiresMountsFor=/sysroot /boot/efi
After=local-fs.target
Before=basic.target final.target ostree-finalize-staged.service
Conflicts=final.target

[Service]
Type=oneshot
RemainAfterExit=yes
SELinuxContext=-{UNCONFINED}
ExecStart=/bin/sh /etc/fspec/esp-sync.sh /sysroot /boot/efi {distro}
# Stops after ostree-finalize-staged.service has written the staged deployment's entry.
ExecStop=/bin/sh /etc/fspec/esp-sync.sh /sysroot /boot/efi {distro}
TimeoutStopSec=5min

[Install]
WantedBy=multi-user.target
"
    )
}

pub(crate) fn guard_service() -> String {
    format!(
        "[Unit]
Description=Hold the boot blessing until sshd and tailscale are up (fspec)
ConditionPathExists={COUNTING}
Wants=network-online.target
After=network-online.target sshd.service tailscaled.service
Before=boot-complete.target

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/bin/sh /etc/fspec/boot-guard.sh
TimeoutStartSec=6min

[Install]
RequiredBy=boot-complete.target
"
    )
}

pub(crate) fn deadline_timer() -> String {
    format!(
        "[Unit]
Description=Reboot a boot that is not blessed in ten minutes (fspec)
ConditionPathExists={COUNTING}

[Timer]
OnBootSec=10min
AccuracySec=10s

[Install]
WantedBy=timers.target
"
    )
}

/// Fedora's policy does not let `init_t` rename files on the ESP, which is what blessing and
/// syncing a boot entry do, so both run unconfined. `init_exec_t` is no entry point into that
/// domain, hence the shell.
const UNCONFINED: &str = "system_u:system_r:unconfined_service_t:s0";

fn bless_dropin() -> String {
    format!(
        "[Service]
SELinuxContext=-{UNCONFINED}
ExecStart=
ExecStart=/bin/sh -c 'exec /usr/lib/systemd/systemd-bless-boot good'
"
    )
}

pub(crate) const DEADLINE_SERVICE: &str = "[Unit]
Description=Reboot a boot that is not blessed in ten minutes (fspec)

[Service]
Type=oneshot
ExecStart=/bin/sh -c 'systemctl is-active -q systemd-bless-boot.service || systemctl reboot'
";

/// The mount options of every btrfs subvolume the new root mounts.
pub(crate) const BTRFS_OPTS: &str = "noatime,compress=zstd:1";

pub(crate) const WATCHDOG: &str = "[Manager]
RuntimeWatchdogSec=60s
";

/// Gives the new root the identity of the root at `$1`: its hostname, ssh host keys,
/// NetworkManager connections and tailscale state, the sudo rules of its wheel group (so the first
/// `fspec apply` runs as it did there), and the users named after `$6` with their uid, gid, gecos,
/// home and password hash, their shell found again in the new root, and their memberships of the
/// groups the new root has. `$2` is the new root's `/usr` parent, `$3` and `$4` its `/etc` and
/// `/var`, `$5` an fstab line for the shared home or empty. Nothing it copies is printed.
pub(crate) const INHERIT: &str = r#"set -eu
umask 077
src=$1 root=$2 etc=$3 var=$4 home=$5
shift 5

for k in "$src"/etc/ssh/ssh_host_*; do
	[ -f "$k" ] || continue
	case $k in *.pub) mode=644 ;; *) mode=600 ;; esac
	install -m "$mode" -o root -g root "$k" "$etc/ssh/${k##*/}"
done
if [ -d "$src/etc/NetworkManager/system-connections" ]; then
	mkdir -p "$etc/NetworkManager/system-connections"
	for c in "$src"/etc/NetworkManager/system-connections/*; do
		[ -f "$c" ] || continue
		install -m 600 -o root -g root "$c" "$etc/NetworkManager/system-connections/"
	done
fi
if [ -d "$src/var/lib/tailscale" ]; then
	mkdir -p "$var/lib"
	rm -rf "$var/lib/tailscale"
	cp -a "$src/var/lib/tailscale" "$var/lib/"
	chmod 700 "$var/lib/tailscale"
fi
# The path $2 inside the root $1, with its symlinks resolved against $1 rather than /: NixOS
# links /etc/hostname to /etc/static, itself a link into /nix/store.
inside() {
	r=$1 p=${2#/} out= n=0
	while [ -n "$p" ]; do
		c=${p%%/*}
		case $p in */*) p=${p#*/} ;; *) p= ;; esac
		case $c in '' | .) continue ;; ..) out=${out%/*}; continue ;; esac
		if [ -L "$r$out/$c" ] && [ "$n" -lt 40 ]; then
			n=$((n + 1)) t=$(readlink "$r$out/$c")
			case $t in /*) out= ;; esac
			p=${t#/}${p:+/$p}
		else
			out=$out/$c
		fi
	done
	printf '%s%s\n' "$r" "$out"
}
f=$(inside "$src" /etc/hostname)
if [ -s "$f" ]; then cat "$f" >"$etc/hostname"; chmod 644 "$etc/hostname"; fi
f=$(inside "$src" /etc/sudoers)
if [ -f "$f" ] && grep -q '^[[:space:]]*%wheel[[:space:]]' "$f"; then
	mkdir -p "$etc/sudoers.d"
	grep '^[[:space:]]*%wheel[[:space:]]' "$f" >"$etc/sudoers.d/50-fspec-inherit"
	chmod 440 "$etc/sudoers.d/50-fspec-inherit"
fi
if [ -n "$home" ]; then
	touch "$etc/fstab"
	at=$(printf '%s\n' "$home" | cut -d' ' -f2)
	grep -q "[[:space:]]$at[[:space:]]" "$etc/fstab" || printf '%s\n' "$home" >>"$etc/fstab"
fi

# The line of $1 in the colon-separated file $2, or nothing.
line() {
	[ -f "$2" ] || return 0
	while IFS= read -r l; do
		case $l in "$1":*) printf '%s\n' "$l"; return 0 ;; esac
	done <"$2"
}
# Replace the line of $1 in $2 with $3, keeping the file's mode and labels.
put() {
	touch "$2"
	{
		while IFS= read -r l; do
			case $l in "$1":*) ;; *) printf '%s\n' "$l" ;; esac
		done <"$2"
		printf '%s\n' "$3"
	} >"$2.fspec"
	cat "$2.fspec" >"$2"
	rm -f "$2.fspec"
}
# The group line $2 with user $1 among its members.
member() {
	IFS=: read -r g pw id members <<-EOF
		$2
	EOF
	case ",$members," in *",$1,"*) printf '%s\n' "$2" ;; ,,) printf '%s:%s:%s:%s\n' "$g" "$pw" "$id" "$1" ;;
	*) printf '%s:%s:%s:%s,%s\n' "$g" "$pw" "$id" "$members" "$1" ;; esac
}

for u in "$@"; do
	pw=$(line "$u" "$src/etc/passwd")
	if [ -z "$pw" ]; then
		echo "the user $u is not in $src/etc/passwd" >&2
		exit 1
	fi
	IFS=: read -r name _ uid gid gecos dir shell <<-EOF
		$pw
	EOF
	login=/bin/bash
	for d in /usr/bin /usr/sbin /bin; do
		if [ -x "$root$d/${shell##*/}" ]; then login=$d/${shell##*/}; break; fi
	done
	put "$u" "$etc/passwd" "$name:x:$uid:$gid:$gecos:$dir:$login"
	sh=$(line "$u" "$src/etc/shadow")
	[ -z "$sh" ] || put "$u" "$etc/shadow" "$sh"
	while IFS=: read -r g _ id members; do
		if [ "$id" = "$gid" ]; then primary=$g; else primary=; fi
		case ",$members," in *",$u,"*) ;; *) [ -n "$primary" ] || continue ;; esac
		have=$(line "$g" "$etc/group")
		[ -n "$have" ] || have=$(line "$g" "$root/usr/lib/group")
		if [ -n "$primary" ]; then
			# The primary group only has to exist, under the source's gid.
			[ -n "$have" ] || put "$g" "$etc/group" "$g:x:$gid:"
			continue
		fi
		[ -n "$have" ] || continue
		put "$g" "$etc/group" "$(member "$u" "$have")"
		if [ -f "$etc/gshadow" ]; then
			gs=$(line "$g" "$etc/gshadow")
			[ -n "$gs" ] || gs="$g:!::"
			IFS=: read -r a b c d <<-EOF
				$gs
			EOF
			case ",$d," in *",$u,"*) ;; ,,) put "$g" "$etc/gshadow" "$a:$b:$c:$u" ;; *) put "$g" "$etc/gshadow" "$a:$b:$c:$d,$u" ;; esac
		fi
	done <"$src/etc/group"
done
"#;

/// Runs [`INHERIT`], given as `$1`, on the one ostree deployment in the stateroot `$4` of the
/// sysroot `$3`, whose directory is named after its commit and so only exists once bootc ran.
const OSTREE_INHERIT: &str = r#"set -eu
script=$1 src=$2 sysroot=$3 stateroot=$4 home=$5
shift 5
state=$sysroot/ostree/deploy/$stateroot
set -- "$state"/deploy/*/etc "$@"
etc=$1
shift
exec sh -c "$script" sh "$src" "${etc%/etc}" "$etc" "$state/var" "$home" "$@"
"#;

/// Labels what the install wrote with the image's own SELinux policy, since the running root may
/// have none. Runs inside the image, where `setfiles` and the policy are.
const RELABEL: &str = r#"set -eu
[ -r /etc/selinux/config ] || exit 0
. /etc/selinux/config
contexts=/etc/selinux/$SELINUXTYPE/contexts/files/file_contexts
command -v setfiles >/dev/null && [ -r "$contexts" ] || exit 0
state=/target/ostree/deploy/$1
for etc in "$state"/deploy/*/etc; do
	setfiles -F -r "${etc%/etc}" "$contexts" "$etc"
done
setfiles -F -r "$state" "$contexts" "$state/var"
# A walk of /var does not descend into the shared home mounted on it.
if mountpoint -q "$state/var/home"; then setfiles -F -r "$state" "$contexts" "$state/var/home"; fi
"#;

/// The kernel options of the running command line that belong to the machine. Only `console=`
/// carries over: the rest can name the running root, like NixOS's `init=`.
fn carried(cmdline: &str) -> Vec<&str> {
    cmdline.split_whitespace().filter(|a| a.starts_with("console=")).collect()
}

/// The mounted ESP systemd-boot reads its entries from, and where it is mounted.
pub(crate) fn esp(m: &Machine) -> Option<(&Part, &str)> {
    m.parts.iter().filter(|p| p.parttype == ESP_TYPE).find_map(|p| {
        ["/efi", "/boot/efi", "/boot"].into_iter().find(|t| p.mounts.iter().any(|at| at == t)).map(|t| (p, t))
    })
}

pub(crate) fn valid_name(name: &str) -> Result<()> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()) {
        bail!("`{name}` cannot name a root; use lowercase letters and digits");
    }
    Ok(())
}

/// What `switch --to-image --beside` installs.
pub struct Beside<'a> {
    /// The locked `repo@sha256:...`.
    pub image: &'a str,
    /// Names the subvolume `@<distro>` and the ESP directory.
    pub distro: &'a str,
    /// The subvolume whose identity the new root keeps, default the running root's.
    pub inherit: Option<&'a str>,
    /// The users the new root gets from it.
    pub users: Vec<String>,
    /// A plain-HTTP registry repository holding the same digest, pulled from instead of the
    /// locked repository, which the new root still updates from.
    pub pull_from: Option<&'a str>,
}

/// The ostree stateroot `bootc install` deploys into when given none.
const STATEROOT: &str = "default";

/// `podman run` of `image` with the new root at `/target`, as bootc runs from its own image.
fn in_image(root: &str, image: &str) -> Cmd {
    Cmd::new("podman")
        .args(["run", "--rm", "--privileged", "--pid=host"])
        .args(["-v", "/dev:/dev", "-v", "/var/lib/containers:/var/lib/containers", "-v"])
        // rbind, so the relabel sees the shared home mounted under the new root.
        .arg(format!("{root}:/target:rbind"))
        .args(["--security-opt", "label=type:unconfined_t"])
        .arg(image)
}

/// Install the bootc image into a new subvolume `@<distro>` beside the running root, on the same
/// btrfs filesystem, keeping the machine's identity from the root `b.inherit`. bootc installs no
/// bootloader: the deployment's kernels go onto the ESP with Boot Loader Specification entries
/// that count three tries, and units in its `/etc` keep them current, hold the blessing until the
/// machine is reachable and reboot a boot that is not. `loader.conf`, the other roots' entries and
/// the EFI variables stay as they are.
pub fn beside(paths: &Paths, m: &Machine, b: &Beside) -> Result<Plan> {
    let distro = b.distro;
    valid_name(distro)?;
    if m.firmware != Firmware::Efi {
        bail!("this machine booted through BIOS; --beside boots the image through systemd-boot on the ESP");
    }
    if m.fstype != "btrfs" {
        bail!("the root filesystem is {}; --beside puts the image in a btrfs subvolume beside it", m.fstype);
    }
    let from = b.inherit.unwrap_or(m.root.trim_start_matches('/'));
    if !(from.is_empty() || from.starts_with('@')) || from.contains('/') || from.contains("..") {
        bail!("--inherit takes a top-level subvolume like @nixos, not `{from}`");
    }
    let Some((esp_part, esp_at)) = esp(m) else { bail!("no ESP is mounted at /efi, /boot/efi or /boot") };
    let (image, origin) = match b.pull_from {
        Some(mirror) => {
            let Some((repo, digest)) = b.image.split_once('@') else {
                bail!("--pull-from needs a locked image pinned by digest, not `{}`", b.image)
            };
            // fspec ci pushes `:latest` next to every digest it locks.
            (format!("{mirror}@{digest}"), Some(format!("{repo}:latest")))
        }
        None => (b.image.to_string(), None),
    };
    let esp_dir = s(&paths.system(esp_at));
    let new = subvol(distro);
    let work = paths.system(WORK);
    let (top, root, stage) = (s(&work.join("top")), s(&work.join(distro)), s(&work.join(format!("{distro}-etc"))));
    let source = if from.is_empty() { top.clone() } else { format!("{top}/{from}") };
    let mut plan = Plan::default();

    plan.expect(
        Cmd::new("test").arg("-e").arg(format!("{esp_dir}/EFI/systemd/systemd-bootx64.efi")),
        true,
        format!("systemd-boot is not installed on the ESP at {esp_dir}"),
    );
    plan.run(Cmd::new("mkdir").arg("-p").arg(&top));
    plan.run(Cmd::new("mount").args(["-o", "subvolid=5"]).arg(format!("UUID={}", m.fs_uuid)).arg(&top));
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
    for u in &b.users {
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
    plan.undo(delete_new(&m.fs_uuid, &new));
    plan.run(Cmd::new("mkdir").arg("-p").arg(&root));
    plan.run(
        Cmd::new("mount")
            .arg("-o")
            .arg(format!("subvol={new},{BTRFS_OPTS}"))
            .arg(format!("UUID={}", m.fs_uuid))
            .arg(&root),
    );
    plan.cleanup.insert(0, Cmd::new("umount").arg("-R").arg(&root));

    let mut kargs = vec![format!("rootflags=subvol={new},{BTRFS_OPTS}")];
    if let Some(luks) = &m.luks {
        kargs.push(format!("rd.luks.uuid={luks}"));
    }
    // A boot that fails in the initramfs reboots instead of waiting at a shell, so the next try
    // or the next entry gets its turn.
    kargs.extend(["panic=10", "rd.shell=0", "rd.emergency=reboot"].map(String::from));
    kargs.extend(carried(&m.cmdline).into_iter().map(String::from));
    if b.pull_from.is_some() {
        plan.run(Cmd::new("podman").args(["pull", "--tls-verify=false", &image]));
    }
    plan.run(
        in_image(&root, &image)
            .args(["bootc", "install", "to-filesystem", "--bootloader=none"])
            // The locked repository is private until it is published, so bootc cannot check it.
            .args(origin.iter().flat_map(|o| [format!("--target-imgref={o}"), "--skip-fetch-check".into()]))
            // Finalizing remounts the target read-only and freezes it, which on a filesystem
            // shared with the running root reaches that root too.
            .arg("--skip-finalize")
            .arg(format!("--root-mount-spec=UUID={}", m.fs_uuid))
            .args(kargs.iter().map(|k| format!("--karg={k}")))
            .arg("/target"),
    );

    let etc = |p: &str| format!("{stage}/{p}");
    plan.run(Cmd::new("rm").args(["-rf", &stage]));
    let files = [
        ("fspec/esp-sync.sh", ESP_SYNC.to_string()),
        ("fspec/boot-guard.sh", GUARD.to_string()),
        ("systemd/system/boot-efi.mount", esp_mount(esp_part)),
        ("systemd/system/fspec-esp-sync.service", sync_service(distro)),
        ("systemd/system/fspec-boot-guard.service", guard_service()),
        ("systemd/system/fspec-boot-deadline.timer", deadline_timer()),
        ("systemd/system/fspec-boot-deadline.service", DEADLINE_SERVICE.to_string()),
        ("systemd/system/systemd-bless-boot.service.d/fspec.conf", bless_dropin()),
        ("systemd/system.conf.d/fspec-watchdog.conf", WATCHDOG.to_string()),
    ];
    for (path, text) in files {
        let at = etc(path);
        plan.run(Cmd::new("mkdir").arg("-p").arg(Path::new(&at).parent().map(s).unwrap_or_default()));
        plan.run(Cmd::new("tee").arg(at).stdin(text));
    }
    let enable = [
        ("local-fs.target.wants", "/etc/systemd/system/boot-efi.mount"),
        ("multi-user.target.wants", "/etc/systemd/system/fspec-esp-sync.service"),
        ("boot-complete.target.requires", "/etc/systemd/system/fspec-boot-guard.service"),
        ("timers.target.wants", "/etc/systemd/system/fspec-boot-deadline.timer"),
        ("multi-user.target.wants", "/usr/lib/systemd/system/sshd.service"),
        ("multi-user.target.wants", "/usr/lib/systemd/system/tailscaled.service"),
    ];
    for (dir, unit) in enable {
        let dir = etc(&format!("systemd/system/{dir}"));
        let name = unit.rsplit('/').next().unwrap_or(unit);
        plan.run(Cmd::new("mkdir").arg("-p").arg(&dir));
        plan.run(Cmd::new("ln").args(["-sf", unit]).arg(format!("{dir}/{name}")));
    }
    // The deployment's directory is named after its commit, which only exists once bootc ran.
    let place = r#"set -- "$1" "$2" "$1"/ostree/deploy/*/deploy/*/etc; [ $# -eq 3 ] && [ -d "$3" ] || { echo "expected one ostree deployment under $1" >&2; exit 1; }; cp -a "$2"/. "$3"/"#;
    plan.run(Cmd::new("sh").args(["-c", place, "sh", &root, &stage]));
    plan.run(Cmd::new("mkdir").arg("-p").arg(format!("{root}/boot/efi")));

    let home = m.facts.home.trim_start_matches('/');
    let fstab = if home.starts_with('@') {
        format!("UUID={} /var/home btrfs subvol={home},{BTRFS_OPTS} 0 0", m.fs_uuid)
    } else {
        String::new()
    };
    plan.run(
        Cmd::new("sh")
            .args(["-c", OSTREE_INHERIT, "sh", INHERIT, &source, &root, STATEROOT, &fstab])
            .args(b.users.iter().map(String::as_str)),
    );
    if home.starts_with('@') {
        // The relabel below then labels the shared home too: sshd cannot read an
        // authorized_keys that a root without SELinux wrote.
        let at = format!("{root}/ostree/deploy/{STATEROOT}/var/home");
        plan.run(Cmd::new("mkdir").arg("-p").arg(&at));
        plan.run(Cmd::new("mount").arg("-o").arg(format!("subvol={home}")).arg(format!("UUID={}", m.fs_uuid)).arg(&at));
    }
    plan.run(in_image(&root, &image).args(["sh", "-c", RELABEL, "sh", STATEROOT]));
    plan.run(Cmd::new("sh").arg(etc("fspec/esp-sync.sh")).arg(&root).arg(&esp_dir).arg(distro));
    plan.run(Cmd::new("rm").args(["-rf", &stage]));
    Ok(plan.private(&s(&work)))
}

/// Delete the subvolume `new` of the filesystem `uuid`, with what was created inside it, such as
/// snapper's `.snapshots`.
pub(crate) fn delete_new(uuid: &str, new: &str) -> Cmd {
    let script = r#"m=$(mktemp -d) && mount -o subvolid=5 "UUID=$1" "$m" || exit 1
btrfs subvolume delete --recursive "$m/$2"; r=$?
umount "$m"; rmdir "$m"; exit $r"#;
    Cmd::new("sh").args(["-c", script, "sh", uuid, new])
}
