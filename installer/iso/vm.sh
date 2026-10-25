#!/usr/bin/env bash
# Creates the fspec-installer libvirt VM from out/formalspec-installer-amd64.iso and opens it in
# virt-manager. Rerun with --fresh to wipe the disk and start over. Needs libvirt, virt-install,
# virt-manager and sudo; run from anywhere.
# virt-manager runs through XWayland (GDK_BACKEND=x11), since its Wayland display draws wrong on
# fractional scaling. virt-install sets on_reboot=destroy for the install phase, which would power
# the VM off when the installer reboots, so --events turns that back into a restart.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
iso=$here/out/formalspec-installer-amd64.iso
name=fspec-installer
export LIBVIRT_DEFAULT_URI=qemu:///system

if [ "${1:-}" = --fresh ] && virsh dominfo "$name" >/dev/null 2>&1; then
	virsh destroy "$name" 2>/dev/null || true
	virsh undefine "$name" --nvram --remove-all-storage
fi
virsh net-info default >/dev/null 2>&1 || virsh net-define /dev/stdin <<'XML'
<network><name>default</name><forward mode='nat'/><bridge name='virbr0'/>
<ip address='192.168.122.1' netmask='255.255.255.0'><dhcp><range start='192.168.122.2' end='192.168.122.254'/></dhcp></ip></network>
XML
virsh net-autostart default >/dev/null
virsh net-start default 2>/dev/null || true

if ! virsh dominfo "$name" >/dev/null 2>&1; then
	sudo install -D -m 0644 "$iso" "/var/lib/libvirt/images/$name.iso"
	virt-install --name "$name" --memory 8192 --vcpus 6 --cpu host-passthrough \
		--boot uefi --osinfo detect=on,require=off \
		--disk size=20,format=qcow2,bus=virtio \
		--cdrom "/var/lib/libvirt/images/$name.iso" \
		--network network=default,model=virtio \
		--events on_reboot=restart \
		--graphics spice --video virtio --noautoconsole
fi
GDK_BACKEND=x11 virt-manager --connect qemu:///system --show-domain-console "$name" &
