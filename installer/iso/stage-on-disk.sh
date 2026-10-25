#!/usr/bin/env bash
# Boots the installer ISO once from a machine's own disk, for machines nobody can reach by hand.
# Run as root on the machine:
#   stage-on-disk.sh ISO OVERLAY.tar [HOSTNAME [EXTRA_OPTIONS]]
# The ISO and the overlay go to /var/lib/fspec-live, the live kernel and initrd to the ESP, and a
# systemd-boot entry with +3 boot counting is set as the one-shot entry for the next boot only.
# toram copies the ISO into RAM so the disk it sits on is free to repartition. Any boot that fails,
# hangs or never comes online reboots into the default entry again; panic=20 covers live-boot
# dropping to its initramfs shell when it cannot find the ISO.
set -euo pipefail

iso=$1
overlay=$2
name=${3:-formalspec}
extra=${4:-}
dir=/var/lib/fspec-live
esp=$(bootctl --print-esp-path)

install -d -m 0700 "$dir"
install -m 0600 "$iso" "$dir/formalspec-installer-amd64.iso"
install -m 0600 "$overlay" "$dir/overlay.tar"

# findiso= and fspec.overlay= are paths from the root of the filesystem that holds them, which
# live-boot mounts at its default subvolume: on btrfs that path starts with the subvolume.
mnt=$(findmnt -n -o TARGET -T "$dir")
fsroot=$(findmnt -n -o FSROOT -T "$dir")
rel=${fsroot%/}${dir#"${mnt%/}"}

iso_mnt=$(mktemp -d)
trap 'umount "$iso_mnt" 2>/dev/null; rmdir "$iso_mnt"' EXIT
mount -o loop,ro "$dir/formalspec-installer-amd64.iso" "$iso_mnt"
install -d "$esp/fspec-live"
install -m 0644 "$iso_mnt/live/vmlinuz" "$esp/fspec-live/vmlinuz"
install -m 0644 "$iso_mnt/live/initrd.img" "$esp/fspec-live/initrd.img"

rm -f "$esp"/loader/entries/fspec-live*.conf
cat > "$esp/loader/entries/fspec-live+3.conf" <<ENTRY
title   FormalSpec installer (one-shot)
linux   /fspec-live/vmlinuz
initrd  /fspec-live/initrd.img
options boot=live components findiso=$rel/formalspec-installer-amd64.iso fspec.overlay=$rel/overlay.tar username=root hostname=$name locales=en_US.UTF-8 keyboard-layouts=us toram panic=20${extra:+ $extra}
ENTRY
sync
bootctl set-oneshot fspec-live.conf
bootctl list --no-pager | grep -A3 'fspec-live'
