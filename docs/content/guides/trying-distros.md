+++
title = "Trying distros"
description = "Boot a distro's live ISO in a throwaway virtual machine with one command, on this Linux host or on another one over ssh, without installing anything."
+++

`fspec try` boots a distro's live ISO in a KVM virtual machine, so you can see and use its
desktop before you [switch a host to it](switching-distros.md). Nothing changes on the host: no
package, no service, no spec change. The ISO is downloaded once into a cache, the VM gets a fresh
disk, and the disk is deleted when the VM stops.

```sh
fspec try fedora                 # a window on this Linux desktop
fspec try tumbleweed --host g815 # runs on g815, the display opens here
```

## What it needs

The host that runs the VM is a Linux machine with:

- `/dev/kvm`, writable by your user (the `kvm` group on most distros).
- `qemu-system-x86_64` and `qemu-img`. On openSUSE that is `qemu-x86` and `qemu-tools`, plus
  `qemu-ui-gtk` for the window.
- OVMF UEFI firmware: `qemu-ovmf-x86_64`, `edk2-ovmf` or `ovmf`.
- `curl` or `wget`.

Declare these in the host's spec like any other package. fspec itself does not need to be on that
host: `--host` sends a self-contained bash script over ssh.

A Mac cannot run the VMs itself. Run them on a Linux host with `--host`; the Mac opens the display
in Screen Sharing.

## Pick what to boot

| Name | Boots |
| --- | --- |
| `tumbleweed` | openSUSE Tumbleweed, KDE Plasma live |
| `fedora` | Fedora Workstation live, GNOME |
| `ubuntu` | Ubuntu desktop, the current LTS |
| `debian` | Debian stable live, GNOME |
| `arch` | Arch Linux install ISO, console only |
| `nixos` | NixOS unstable graphical installer |
| `aerynos` | AerynOS live, GNOME |
| `void` | Void Linux live, Xfce |

Each name resolves to the project's current ISO from the checksum list or release index the project
publishes, and the download is checked against that checksum. Instead of a name you can pass an
`https://` URL of any ISO, or the path of an ISO on the host that runs the VM.

`fspec try --list` prints these names, and on the VM's host the kept VMs, the running VMs and the
cached ISOs.

## Throwaway and kept VMs

By default every run is throwaway. The VM gets a new sparse disk of 32 GiB (`--disk` changes it),
and when it stops, by Ctrl-C in the terminal, by powering off the guest, or by the ssh connection
dropping, fspec deletes its disk and run directory.

To install the distro inside the VM and come back to it, name it:

```sh
fspec try fedora --keep fedora-test   # boot the ISO, install onto the kept disk
fspec try --keep fedora-test          # boot the installed system again
fspec try --delete fedora-test        # delete it
```

| Where, on the VM's host | Holds |
| --- | --- |
| `~/.cache/fspec/try/iso/<distro>/` | The cached ISO. A newer release replaces the older one, and one no run has booted for 7 days is deleted |
| `~/.cache/fspec/try/disks/` | Throwaway disks while their VM runs |
| `~/.local/share/fspec/try/<name>/` | A kept VM's disk and UEFI variables |
| `/tmp/fspec-try-<id>/` | A running VM: its sockets, links to its files, its VNC password |

A run killed without its cleanup, by a power cut for example, is cleaned up by the next run on that
host. Each run also deletes cached ISOs that no run has booted for 7 days, unless a running VM boots
them. To expire them on a host where you stop using `fspec try`, add a user `tmpfiles.d` rule such
as `e %C/fspec/try/iso - - - am:7d`.

The disk comes before the ISO in the boot order. A new disk is empty, so the VM boots the ISO; once
an installer has written a bootloader to it and reboots, the VM boots the installed system. With
`--keep`, the next `fspec try --keep <name>` boots it too.

## The display

On a Linux desktop the VM opens in a GTK window with virgl, so the guest's OpenGL desktop is
accelerated by the host GPU. Guest Vulkan stays software (llvmpipe): Venus, which forwards Vulkan
to the host GPU, froze the guest display on an NVIDIA host. Closing the window stops the VM.

With `--host`, the VM has no window on that host. qemu serves the display over VNC on a unix
socket only your user can reach, with a random password, and fspec forwards it over the same ssh
connection to a local port. Once the VM answers, fspec opens a viewer: Screen Sharing on macOS,
`remote-viewer` (virt-viewer) or a TigerVNC `vncviewer` on Linux. Without a viewer, or with
`--headless`, it prints the `vnc://` address to open yourself.

The defaults are half the host's CPU threads (2 to 16) and a third of its memory (4 to 16 GiB).
`--cpus` and `--memory <GiB>` override them.

## Disk speed

The disk is virtio on its own I/O thread with io_uring where qemu has it. A throwaway disk skips
the guest's flushes, since it is deleted anyway. A kept disk bypasses the host page cache where
the filesystem allows it, and on btrfs both are created without copy on write.

## Look before it runs

`fspec try <name> --print` resolves the ISO and prints the script that would run on the VM's host,
including the full qemu command line, and runs nothing.
