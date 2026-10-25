+++
title = "Switching distros"
description = "Move a host to another distro, or another bootc image, while your home and FormalSpec's store carry over, and get a disk onto a layout that holds several roots."
+++

The `system` block is the only part of a spec that names a distro. Packages from FormalSpec's own
sources, Flatpaks, files, secrets and user services are the same on every distro, so moving a host
to another distro means changing that block and installing the new root. FormalSpec installs the
new distro beside the running one, on the same disk, and both stay bootable until you delete the
old one.

## Before you start

These commands repartition disks, convert filesystems, delete whole roots and change which system
your machine boots. Read this section before you run any of them.

bootc images and `fspec switch` have only run against test roots so far. `fspec disk`,
`fspec switch` and `fspec forget` have not been through enough real machines to trust with data
you cannot restore. Run them on a machine you can reinstall, with a backup you have checked.

| Command | What it destroys | What protects you |
| --- | --- | --- |
| `fspec disk init <device>` | Everything on the disk | You type the device name. A disk with data needs `--backup` |
| `fspec disk migrate` | Changes partitions and moves files in place | You type the disk name for each destructive step, and it needs a recent snapshot or `--backup` |
| `fspec disk migrate <device>` | Converts an ext4 root to btrfs | You type the device name. It needs `--backup` or `--no-backup` |
| `fspec switch --to-image --yes` | Replaces the running system and reinitialises `/boot` | `--yes` |
| `fspec switch --reset-var` | Deletes everything in `/etc` and `/var` that is not kept, at the next boot | Removing the `pending` marker cancels it |
| `fspec forget <distro>` | Deletes that root and its boot entries | Refuses the running root, and refuses unless this root owns boot |
| `fspec switch --distro`, `--beside` | Nothing outside the new root | Refuses a name that already exists |

Have these ready before a switch:

- A backup of everything you care about, on another disk.
- A way to boot something else: the [installer ISO](installer.md) on a USB stick, which is a
  live Linux system with `fspec` and every bootstrapper on it.
- Physical access to the machine, or a console, in case it does not come back on the network.

[Recover from a failed switch](#recover-from-a-failed-switch) covers what to do when a new root
does not boot.

## What carries over

FormalSpec never manages personal files. It only touches paths it declared or attributed to a
package. Declare `protect` to make it refuse any action under your homes:

```starlark
host(
    name = "desktop",
    platform = "x86_64-linux",
    system = fedora(packages = ["NetworkManager", "sudo"]),
    users = [user("alex", uid = 1000, shell = "fish")],
    protect = ["/home", "/var/home"],
    persist = ["/var/lib/libvirt"],
    modules = [...],
)
```

- `protect` has no default: the list is exactly what you declare. A `file`, `secret` or package
  `data` path under an entry fails the plan with `<path> is under protected path /home`. Your own
  home stays usable, since the spec manages files there; other users' homes stay refused.
- `uid` on `user(...)` pins the account's UID, so file ownership in the shared home is right in
  every root.
- `persist` lists paths that `fspec switch --reset-var` keeps on an image host.

On the default disk layout, `/home` and the store at `/opt/fspec` are shared subvolumes that every
root mounts. A new root finds your home and every store package already there, and does not
download the store again.

## Switch between bootc images

On an [image host](system-layer.md#image-hosts), a different distro is a different base image.

1. Change `base` in the host's `image(...)`:

   ```starlark
   system = image(base = "ghcr.io/ublue-os/aurora:stable"),
   ```

2. Commit and push. The `fspec-images` workflow builds the new image and opens a pull request
   with its digest.

3. Merge the pull request, pull it, and apply:

   ```sh
   git pull
   fspec apply
   ```

   Apply runs `bootc switch` and stages the new image.

4. Reboot. The machine boots the new distro, and the old one is still in the boot menu.

Your home, Flatpaks, store packages and user services sit outside the image and carry over.

### Reset /etc and /var to the new image

bootc keeps `/etc` and `/var` across a switch, so configuration from the old distro stays. To
start from the new image's defaults:

1. After the apply that staged the new image, run:

   ```console
   $ sudo fspec switch --reset-var
   /etc and /var reset at the next boot; remove /var/lib/fspec/reset-var/pending to cancel
   ```

2. Reboot.

The running system cannot reset its own `/etc`, so the command only stages the reset. It writes a
keep list, a script and a `pending` marker under `/var/lib/fspec/reset-var/`, and enables
`fspec-reset-var.service`, which runs early in the next boot. The script deletes everything under
`/var` and `/etc` that is not kept, copies `/usr/etc` over `/etc`, and removes `pending` last, so
an interrupted run repeats on the next boot.

Always kept:

| Path | Why |
| --- | --- |
| `/var/lib/fspec`, `/opt/fspec`, `/var/opt/fspec` | FormalSpec's state and store |
| `/home`, `/var/home`, `/var/roothome` | The homes |
| `/etc/passwd`, `/etc/shadow`, `/etc/group`, `/etc/gshadow`, `/etc/subuid`, `/etc/subgid` | Accounts and passwords. Apply recreates declared users, but not their passwords |
| The host's `persist` paths | You asked for them |

To cancel before you reboot:

```sh
sudo rm /var/lib/fspec/reset-var/pending
```

## Switch to a classic distro

`fspec switch --distro` installs a classic distro as a new root beside the running one. It takes
`fedora`, `arch`, `debian`, `ubuntu`, `void` or `aerynos`. The disk must be on one of the
[supported layouts](#disk-layouts); `fspec disk plan` gets it there.

1. Change the host's `system` to the target distro, with that distro's package names:

   ```starlark
   system = void(packages = ["base-system", "NetworkManager", "dbus", "sudo"]),
   ```

   The new root is built from this block. Nothing is read from a second declaration.

2. Check that the switch fits the disk. Run as root, `plan` prints the layout, which root owns
   boot, and whether the target fits, instead of the usual plan:

   ```console
   $ sudo fspec plan
   host desktop declares void and this machine runs fedora: `fspec switch --distro void` installs it beside this root
   disk: the default layout
   boot is owned by fedora, this root
   switch --distro void: fits this disk
   ```

3. Install the new root:

   ```sh
   sudo fspec switch --distro void --host desktop
   ```

   Fedora needs `--release <number>`. For Debian and Ubuntu, `--release` names the suite; the
   defaults are `stable` and `noble`. The running system needs the target's bootstrapper on its
   `PATH`: `dnf`, `pacstrap`, `debootstrap`, `xbps-install`, or for AerynOS the static `moss`
   from AerynOS's os-tools releases.

   When it finishes:

   ```text
   void is installed beside fedora: reboot and pick "Void (fspec)" from the boot menu. Run `fspec switch --adopt-boot` there to hand boot over before you `fspec forget` this root
   ```

4. Reboot and pick the new entry from the boot menu.

5. Log in and apply. The switch applied only the system scope, so this first apply in the new
   root writes your user files and services:

   ```sh
   fspec apply
   ```

6. Make the new root own boot, so it starts by default:

   ```sh
   sudo fspec switch --adopt-boot
   ```

7. When you no longer need the old root, delete it from the new one:

   ```sh
   sudo fspec forget fedora
   ```

The command refuses `--distro` for a host whose `system` does not already declare that distro.

### Who owns boot

Exactly one root owns boot: the one the machine starts by default. On the default layout that is
the `default` line in `/efi/loader/loader.conf`, on ZFS the pool's `bootfs`, and on the BIOS
layout the root whose GRUB the boot sector loads. The first switch on a machine records the
running root as the owner.

`fspec switch --adopt-boot`, run from the root you booted, makes it the owner:

| Layout | What it does |
| --- | --- |
| Default | Writes `default <distro>-*` to `loader.conf` and runs `bootctl set-default ""`, so no EFI variable outranks it |
| ZFS | `zpool set bootfs=rpool/ROOT/<distro> rpool` |
| BIOS | Installs this root's GRUB to the boot sector and writes a menu that lists every other root, then records this root as the owner |

On a bootc root, bootupd owns the bootloader, and `--adopt-boot` refuses.

### Delete an old root

`fspec forget <distro>` deletes a root that `fspec switch` created, or the NixOS that a
[migration](#migrate-a-nixos-ext4-machine-in-place) kept in `@nixos`. It removes the root's
kernels and boot entries from the ESP, or its folder in the shared boot partition, then deletes
the root with `btrfs subvolume delete --recursive` or `zfs destroy -r`. Every system file the
old distro left behind goes with it.

It refuses:

- the root you booted;
- any root while the running root does not own boot. Run `fspec switch --adopt-boot` first, so
  the bootloader's default never goes with a deleted root.

`fspec forget aerynos` also deletes `/efi/EFI/aerynos`, and `fspec forget nixos` deletes
`EFI/nixos` and the `nixos-*.conf` entries.

## Install beside the running root

`--beside <name>` installs a new root into the subvolume `@<name>` on the running root's btrfs
filesystem and leaves everything else alone: the running root, `loader.conf`, the other roots'
entries and the EFI variables. It works on a disk that is not on the default layout yet. It needs
EFI, a btrfs root, and systemd-boot on an ESP mounted at `/efi`, `/boot/efi` or `/boot`. It
refuses a name that already exists.

| Host declares | Command | Installs |
| --- | --- | --- |
| `image(...)` | `fspec switch --to-image --beside <name>` | The locked image, with `bootc install to-filesystem` in podman |
| `tumbleweed()` | `fspec switch --beside <name>` | openSUSE Tumbleweed at the snapshot `fspec.lock` pins, with zypper in the official Tumbleweed container |

1. Lock what you install. An image host needs a digest (`fspec update --images`), a Tumbleweed
   host a snapshot (`fspec update`).

2. Install:

   ```sh
   sudo fspec switch --beside tw --host laptop
   ```

3. Boot it once, without making it the default. The command prints the entry to use:

   ```sh
   sudo bootctl list
   sudo bootctl set-oneshot tw-<kernel>.conf
   reboot
   ```

   `bootctl list` shows the entry's exact name. For an image, the first entry is
   `<name>-ostree-1.conf`.

The new root keeps the identity of the root `--inherit @<subvolume>` names, by default the running
one: its ssh host keys, NetworkManager connections, `/var/lib/tailscale`, and the host's declared
users with their UID, groups, home and password hash. The running root's home subvolume is
mounted in the new root as `/var/home`.

A trial boot counts tries. The new entry gets three, and a boot that fails in the initramfs
reboots instead of waiting at a shell. A boot is blessed only once `sshd` listens and
`tailscale status` reports the node online. A Tumbleweed root waits for tailscale only when the
inherited root had tailscale state. An unblessed boot
reboots after ten minutes, and a hung one after 60 seconds through the watchdog. After three
failed tries, systemd-boot goes back to the default entry. Making the new root the default is a
separate step you take by hand once it has booted and been blessed. An entry set as `default`
boots even after it has run out of tries.

`--device <btrfs partition>` with `--inherit @<root>` installs Tumbleweed from a live system that
does not run from that filesystem. `--pull-from <repository>` pulls an image from a plain-HTTP
registry mirror; the new root still updates from the locked repository.

## Convert a classic machine to an image

```sh
sudo fspec switch --to-image --yes
```

This replaces the running classic system with the host's locked bootc image, using
`bootc install to-existing-root`. It reinitialises `/boot`, so it needs `--yes`. It needs an
`image(...)` host with a digest in `fspec.lock`, refuses a machine that already runs bootc, and
needs the default EFI layout: bootc cannot install to a ZFS root or the shared BIOS boot
partition. Prefer `--to-image --beside <name>`, which leaves the running system in place.

## Disk layouts

Several roots share one disk and one bootloader. A switch needs the disk on one of three layouts.

### Default

The one `fspec disk init` creates and the one to use:

| Part | What it is |
| --- | --- |
| GPT partition table | |
| 2 GiB EFI system partition (FAT32) | Mounted at `/efi` in every root, shared by every distro and by Windows if it is there |
| LUKS2 container (optional) | Holds the rest of the disk |
| One btrfs filesystem | Subvolumes `@<distro>` (one per root), `@home`, `@swap` (holding `swapfile`), `@snapshots`, and `@fspec` mounted at `/opt/fspec` |
| systemd-boot on the ESP | Each distro's kernels go to the ESP under its own name, so roots never overwrite each other's kernels |

Each root keeps its own `/boot` for its packages, and its own kernel tooling writes the ESP:
`kernel-install` with unified kernel images on Fedora, Debian and Ubuntu, a mkinitcpio preset on
Arch, a kernel hook fspec installs on Void, and blsforme on AerynOS. Each root's kernel command
line comes from its `/etc/kernel/cmdline`.

### ZFS

A pool `rpool` with `rpool/ROOT/<distro>` boot environments, `rpool/home` and `rpool/fspec`,
booted by ZFSBootMenu from the ESP. Debian, Ubuntu and Void ship the ZFS module; Arch, Fedora and
AerynOS are refused on ZFS. The distros do not sign out-of-tree ZFS modules, Ubuntu excepted, so
Secure Boot, and with it EAC Linux v2, only works with Ubuntu on ZFS. `plan` says so. ZFS takes
no `--luks`, since it encrypts datasets itself.

### BIOS

For machines without UEFI or with an MBR disk: GRUB in BIOS mode and a shared ext4 boot partition
mounted at `/var/lib/fspec/boot` in every root, each root using its own folder in it as `/boot`.
There are no unified kernel images, and no Secure Boot, so EAC cannot work. AerynOS is refused.

### What each distro needs

| Distro | Smallest ESP | ZFS | BIOS |
| --- | --- | --- | --- |
| Fedora | 1 GiB | No | Yes |
| Arch | 512 MiB | No | Yes |
| Debian | 512 MiB | Yes | Yes |
| Ubuntu | 512 MiB | Yes | Yes |
| Void | 256 MiB | Yes | Yes |
| AerynOS | 1 GiB | No | No |

A switch also checks that the ESP or the shared boot partition has room for the new distro's
kernels, and refuses with the fix when it does not.

## Get a disk onto a supported layout

### See what is needed

```sh
sudo fspec disk plan
```

`disk plan` prints the numbered steps that bring the running disk onto a supported layout, and
marks the destructive ones. It changes nothing. On a disk that is already there:

```text
this disk is on the default layout; nothing to migrate
```

### Migrate in place

```sh
sudo fspec disk migrate
```

`disk migrate` runs the steps that can be done on the running system: adding the shared
subvolumes and moving `/home` and `/opt/fspec` into them, moving a root at the top level into
`@<distro>`, putting the root's kernels on the ESP, and carving a 2 GiB ESP out of the btrfs
partition when the old one is too small. The new ESP keeps the old one's partition GUID and FAT
serial, so firmware entries and every root's `fstab` keep finding it. The old ESP stays as a copy.

`disk migrate` does not encrypt an existing disk, and does not shrink a btrfs filesystem inside
LUKS. Both need `fspec disk init` on another disk.

After it finishes, reboot and run `fspec disk plan` again. It lists what the move left behind,
such as the old root's files and the copies of `/home` and `/opt/fspec`, as destructive steps.

### Convert an ext4 root

An ext4 root converts to btrfs only while it is not mounted:

1. Boot a live system, such as the installer ISO.
2. See the steps:

   ```sh
   sudo fspec disk plan /dev/nvme0n1p2
   ```

3. Convert, naming a backup:

   ```sh
   sudo fspec disk migrate /dev/nvme0n1p2 --backup /mnt/backup/desktop.tar
   ```

   It keeps the filesystem UUID, fixes the root's `fstab` line and rebuilds its initramfs and
   GRUB configuration.

4. Boot the converted root and run `sudo fspec disk migrate` for the rest of the steps.

### Start from a blank disk

```sh
sudo fspec disk init /dev/sdb --luks --distro debian --host server
```

`disk init` partitions and formats a disk from scratch, creates the shared subvolumes or
datasets, installs the bootloader, and with `--distro` and `--host` installs the first root the
same way `switch --distro` does.

| Flag | Default | Meaning |
| --- | --- | --- |
| `--layout` | `default` | `default`, `zfs` or `bios` |
| `--luks` | off | Put the btrfs filesystem in LUKS2 |
| `--distro`, `--host` | none | Install this distro as the first root, from this host's declaration |
| `--release` | per distro | Fedora release number, or Debian or Ubuntu suite |
| `--swap` | `4` | Size of the swapfile in `@swap`, in GiB |
| `--backup` | none | A backup of the disk's contents; required when the disk is not blank |

It refuses a partition (pass the whole disk), a disk with anything mounted, and a disk smaller
than 16 GiB.

### Safety checks

Every destructive step:

- asks you to type the device name back:

  ```text
  This erases everything on /dev/sdb.
  Type /dev/sdb to continue:
  ```

  Anything else stops it with `you typed `<text>`, not /dev/sdb; nothing was changed`;
- refuses a mounted target, except for the steps built for one (resizing btrfs and moving
  subvolumes run on the mounted filesystem);
- checks for a backup first: a read-only snapshot under `@snapshots` taken in the last 24 hours,
  or `--backup <path>` naming a file or directory modified in the last 24 hours. A blank disk
  needs neither.

The backup check only looks at the timestamp. It does not check that the backup is complete or
that it restores.

## Migrate a NixOS ext4 machine in place

A NixOS root on ext4 can move onto the default layout without a reinstall. NixOS stays bootable
the whole time, and a failure puts it back on ext4 as it was.

NixOS writes its `fstab` and initrd from its configuration, and every older generation mounts `/`
as ext4. So you build the new system first, and fspec converts the disk under it.

Prerequisites:

- A NixOS system built to mount `/` as btrfs `subvol=@nixos` on the same filesystem UUID, `/home`
  from `@home` and `/swap` from `@swap`, copied into the ext4 root's `/nix/store`
  (`nix copy --to`).
- A systemd-boot entry `loader/entries/fspec-live*.conf` on the ESP that boots a live system,
  such as the installer ISO, from the disk. fspec does not write this entry.
- The EFI variable `LoaderEntryDefault` cleared: `bootctl set-default ""`.

Steps:

1. Boot the `fspec-live` entry. The NixOS root must not be mounted.

2. See the steps:

   ```sh
   sudo fspec disk plan /dev/nvme0n1p2 --nixos-system /nix/store/<hash>-nixos-system-desktop-<version>
   ```

3. Run them:

   ```sh
   sudo fspec disk migrate /dev/nvme0n1p2 --nixos-system /nix/store/<hash>-nixos-system-desktop-<version> --backup /mnt/backup/desktop.tar
   ```

   Type the device name when asked. `--no-backup` converts without one, for a disk with nothing
   to save.

4. Reboot. The new NixOS generation boots from `@nixos`.

5. From the booted `@nixos`, see what is left to clean up:

   ```sh
   sudo fspec disk plan
   ```

   It lists deleting the old top level with `ext2_saved`, the copy of `/home` in `@nixos` and its
   old `/swapfile`. Deleting `ext2_saved` ends the way back to ext4, so do it last, once you trust
   the new layout.

What the migration does:

- Makes `fspec-live*` the default entry first, so a reset during the migration comes back into
  the live system.
- Converts with `btrfs-convert --uuid copy`, keeping `ext2_saved`, snapshots the top level into
  `@nixos`, and creates `@home`, `@swap`, `@snapshots` and `@fspec`.
- Runs NixOS's own `switch-to-configuration boot` in a chroot, so NixOS writes its entry. The
  older generations cannot boot btrfs, so their entries move to `loader/entries.ext4/` on the ESP.
- Gives the new entry three tries. A failed initrd reboots and uses a try; a failed mount after
  the initrd still reaches `sshd` instead of an emergency shell. After three failed boots,
  systemd-boot falls through to `fspec-live`. Once NixOS marks a boot good, those trial options
  are dropped again. `sudo fspec disk unmask` drops them by hand.
- Starts `fspec-compress.service`, which recompresses `@home` and `@nixos` with zstd until the
  filesystem is 80% full or the live system reboots.

### How a failed NixOS migration rolls back

Every step records its undo, synced to disk before the step runs, in `fspec/disk-journal` on the
ESP. When a step fails, `disk migrate` rolls back at once: it runs `btrfs-convert -r` and puts the
ESP's `loader`, `EFI/systemd` and `EFI/BOOT` back, so NixOS boots from ext4 as before. The error
ends with `rolled back: <device> is ext4 again, with NixOS as it was`.

When the migration was killed or the machine reset, boot `fspec-live` again. `disk plan` reports
the interrupted run:

```text
an interrupted migration left <esp>/fspec/disk-journal; `fspec disk migrate /dev/nvme0n1p2` rolls it back
```

Run that command to roll it back.

After a successful migration, and until you delete `ext2_saved`, `btrfs-convert -r <device>`
from a live system still goes back to ext4.

## Recover from a failed switch

**The new root does not boot.** Reboot, and pick the previous root from the systemd-boot menu. A
`switch --distro` never changes the old root, and a `--beside` install never changes the default
entry, so the old root boots as before. After three failed tries, a trial entry stops being
picked on its own.

**No root boots.** Boot the installer ISO from USB, or the `fspec-live` entry if your ESP has
one. From there, mount the btrfs filesystem with `subvolid=5` to reach every root, your home in
`@home` and the snapshots in `@snapshots`.

**An image update broke the system.** Pick the previous deployment in the boot menu, or run
`sudo bootc rollback` and reboot.

**A `--reset-var` is staged by mistake.** Remove `/var/lib/fspec/reset-var/pending` before you
reboot.

**A NixOS migration failed.** See
[How a failed NixOS migration rolls back](#how-a-failed-nixos-migration-rolls-back).

## How a switch works

`fspec switch --distro` reads the machine (`lsblk`, `findmnt`, `/sys/firmware/efi`, the running
root and its kernel command line), then:

1. Checks the target distro against the layout, and stops with the fix if it does not fit. On the
   BIOS layout, it refuses unless the running root owns boot.
2. Creates the root, `@<distro>` or `rpool/ROOT/<distro>`, and mounts it under
   `/run/fspec/switch/`.
3. Bootstraps the base system from the running OS with `dnf --installroot`, `pacstrap -K`,
   `debootstrap`, `xbps-install -r` or `moss -D`, including the layout's boot packages.
4. Writes the new root's `fstab`, `crypttab`, `/etc/kernel/cmdline` and kernel tooling
   configuration, and copies `resolv.conf`. The command line keeps the running kernel's machine
   options, such as `console=` and `net.ifnames=0`, with only the root swapped.
5. Copies the running `fspec` to `/usr/local/bin/fspec` in the new root and the spec to
   `/var/lib/fspec/spec`, binds the shared `/home`, `/opt/fspec` and `/efi` in, and runs the
   system scope of the apply in a chroot:

   ```sh
   chroot <new root> /usr/local/bin/fspec --spec /var/lib/fspec/spec/spec.star apply --local --scope system --host <host>
   ```

   The new distro's own backends do the work. The `fspec` binary must run in the fresh root, so it
   has to be a static build. A glibc or Nix build fails there.
6. Runs the distro's kernel tooling once, which puts its kernels where the layout boots them.
7. Unmounts, also after a failure.

Under `--root`, the probes are skipped and the default layout is assumed, so the commands are
logged to `<root>/var/log/fspec-commands.log` without running.

## Troubleshooting

**`host desktop does not declare arch as its system. Edit `system =` to arch(...) first: the new root is built from that block`**

Change the host's `system` to the target distro before you run `switch --distro`.

**`cannot switch to fedora: <reason>`**

The disk does not fit the target. The reason names the fix, for example
`the ESP is 512 MiB and fedora needs 1024 MiB; run `fspec disk migrate` to move it to a 2 GiB one`.

**`this disk is not on a supported layout: <reasons>; run `fspec disk plan` for the steps`**

Run `sudo fspec disk plan` and follow its steps.

**`fedora needs --release <number>, the Fedora release to install`**

Pass `--release`, for example `--release 44`.

**`boot is owned by fedora, not this root (void); run `fspec switch --adopt-boot` from void first`**

`forget` runs only from the root that owns boot. Run `sudo fspec switch --adopt-boot` there.

**`fedora is the system you booted; boot another distro before forgetting it`**

Boot the root you keep, then forget the other one.

**`this is a bootc deployment, where bootupd owns the bootloader; switch --adopt-boot does not handle bootc roots yet`**

On a bootc root, set the default entry by hand.

**`no image is staged; change image(base = ...), run `fspec apply`, then switch --reset-var`**

`--reset-var` resets to the staged image. Stage one first.

**`--beside installs the host's image with --to-image, or a host that declares tumbleweed()`**

`--beside` without `--to-image` installs Tumbleweed only. For another classic distro, use
`--distro`.

**`<device> is smaller than 16 GiB, too small for a 2 GiB ESP and a root`**

Use a larger disk.

**`<device> holds data; name a backup taken in the last 24 hours with --backup`**

`disk init` erases the disk. Take a backup and name it, or use a blank disk.

**`the backup <path> is <n> hours old; take a new one first`**

Backups older than 24 hours do not count.

**`<device> holds NixOS, whose fstab and initrd come from its configuration: build a system that mounts / as btrfs subvol=@nixos, copy it into this root's /nix/store, and name it with --nixos-system`**

See [Migrate a NixOS ext4 machine in place](#migrate-a-nixos-ext4-machine-in-place).

**`the ESP has no fspec-live entry to boot while the migration runs; install the live entry first`**

Add a systemd-boot entry that boots a live system from the disk, named `fspec-live*.conf`.

**`the EFI variable LoaderEntryDefault outranks loader.conf; clear it with `bootctl set-default ""` first`**

Run `sudo bootctl set-default ""`.
