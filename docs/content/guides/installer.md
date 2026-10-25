+++
title = "The installer"
description = "Boot the FormalSpec installer ISO, install a new machine from a distro and a preset or from an existing spec, and what you find on disk afterwards."
+++

The installer puts a FormalSpec-managed Linux system on a blank machine. You boot it from a USB
stick, answer a few screens, and it partitions the disk, installs the distro you picked, writes a
spec for the machine and applies it. Use it for a new machine, or to reinstall one from the spec
you already keep in git.

The installer erases the disk you install to. It installs one system per disk, on the
[default layout](switching-distros.md#default), and does not install beside Windows or another
system on the same disk.

## What the ISO is

The ISO is a Debian live system. It boots with Secure Boot on, through Debian's signed shim. It
carries:

- `fspec`, built static so it runs inside any freshly installed root;
- `fspec-install`, the installer itself, a full-screen terminal program;
- the tools each distro installs with: debootstrap, dnf, pacman with arch-install-scripts, a
  static xbps, a static moss, and podman;
- disk tools (gdisk, parted, btrfs-progs, cryptsetup), NetworkManager, git and an ssh client.

On a machine with a GPU, the installer runs full screen in a terminal under a small Wayland
compositor, with true colour, a proper font and the mouse. Without a GPU, or on a serial console,
it runs on the console in 16 colours. Every screen works with the keyboard alone.

## Get the ISO

When a release is published, the ISO and its checksums are attached to it:

- <https://github.com/FormalStudio/formalspec/releases/latest/download/formalspec-installer-amd64.iso>
- <https://github.com/FormalStudio/formalspec/releases/latest/download/SHA256SUMS>

The ISO is for x86_64 machines.

1. Download both files into one directory:

   ```sh
   curl -LO https://github.com/FormalStudio/formalspec/releases/latest/download/formalspec-installer-amd64.iso
   curl -LO https://github.com/FormalStudio/formalspec/releases/latest/download/SHA256SUMS
   ```

2. Check the download:

   ```console
   $ sha256sum -c SHA256SUMS
   formalspec-installer-amd64.iso: OK
   ```

   On macOS, use `shasum -a 256 -c SHA256SUMS`.

## Write it to a USB stick

Writing the ISO erases the USB stick.

On Linux:

1. Plug in the stick and find its device name. Compare the list before and after you plug it in:

   ```sh
   lsblk -d -o NAME,SIZE,MODEL,TRAN
   ```

2. Write the ISO, replacing `/dev/sdX` with the stick:

   ```sh
   sudo dd if=formalspec-installer-amd64.iso of=/dev/sdX bs=4M status=progress conv=fsync
   ```

On macOS:

1. Find the stick:

   ```sh
   diskutil list external
   ```

2. Unmount it and write the ISO, replacing `diskN` with the stick:

   ```sh
   diskutil unmountDisk /dev/diskN
   sudo dd if=formalspec-installer-amd64.iso of=/dev/rdiskN bs=4m
   ```

Graphical tools that write disk images, such as GNOME Disks or balenaEtcher, work too.

## Boot it

1. Plug the stick into the machine you want to install, and open its boot menu. The key differs
   per vendor; F12, F11, F8 and Esc are common.
2. Pick the USB stick.
3. Pick an entry from the installer's menu:

   | Entry | Use it for |
   | --- | --- |
   | FormalSpec installer | Most machines |
   | FormalSpec installer (serial console, ttyS0 115200) | Servers and VMs without a screen |
   | FormalSpec installer (shell only) | A root shell instead of the installer |
   | FormalSpec installer (fail-safe) | Graphics or ACPI problems with the default entry |

The installer starts on its own, logged in as root.

To install over the network, connect first: the live system runs NetworkManager, so `nmtui` in
the shell sets up Wi-Fi. A wired connection with DHCP works without setup.

## Install a new system

The installer writes a spec for you from a distro and a preset. The right side of the screen shows
the `spec.star` it is writing as you choose.

### Welcome

Pick **Install a new system**.

### Language

Choose the language, the keyboard layout and the time zone. Type in a column to filter it. The
**Test your keyboard** field checks the layout before you type a password.

### Disk

Choose the disk to install to. The list shows each disk's model, size, connection and what is on
it now. The installer USB itself is marked and cannot be picked. A disk needs at least 16 GB.

- **Encrypt the disk** puts the system in a LUKS2 container. Enter the passphrase twice; it needs
  at least 8 characters. You type it at every boot.
- **Swap size** sets the swapfile, 4 GB by default.

The screen shows the layout it will create: a 2 GB EFI partition and a btrfs filesystem with the
subvolumes for the system, `@home`, `@swap`, `@snapshots` and `@fspec`. The
[default layout](switching-distros.md#default) explains each one.

### System

Choose the distro. Each entry says how it updates, its init system, whether its kernel is signed
for Secure Boot, and whether it is a bootc image. Distros still in development are marked.

| Distro | Updates | Init | Signed kernel | Kind |
| --- | --- | --- | --- | --- |
| Fedora | Releases | systemd | Yes | Classic |
| Bazzite | Releases | systemd | Yes | Image |
| Fedora Atomic | Releases | systemd | Yes | Image |
| Aurora | Releases | systemd | Yes | Image |
| Arch Linux | Rolling | systemd | No | Classic |
| CachyOS | Rolling | systemd | No | Classic |
| Ubuntu | Releases | systemd | Yes | Classic |
| Debian | Releases | systemd | Yes | Classic |
| Void Linux | Rolling | runit | No | Classic |
| Void Linux (musl) | Rolling | runit | No | Classic |
| openSUSE Tumbleweed | Rolling | systemd | Yes | Classic |
| openSUSE Leap | Releases | systemd | Yes | Classic |
| openSUSE Aeon | Rolling | systemd | Yes | Atomic, release candidate |
| openSUSE Kalpa | Rolling | systemd | Yes | Atomic, alpha |
| AerynOS | Rolling | systemd | No | Atomic, alpha |

Image distros and AerynOS need a machine that booted with UEFI. Not every distro listed installs
yet; see [Limits](#limits). [The system layer](system-layer.md) compares the kinds in depth.

### Preset

Choose what goes on the system. A preset is a Starlark module the installer copies into your
spec, so you can change all of it later.

| Preset | Contains |
| --- | --- |
| Minimal | A console system with networking, sudo and nano |
| Desktop | A desktop (GNOME or KDE Plasma, and COSMIC on AerynOS) with a browser, audio, printing and Flatpak from Flathub |
| Gaming | The desktop plus Steam, Proton, GameMode and MangoHud |
| Server | SSH, a firewall and automatic security updates, no desktop |

Use the left and right arrows to pick the desktop. Each distro offers only the presets and
desktops it supports.

### Account

Enter your name, a login name and the computer's name, and a password twice. The login name
starts with a letter and uses lowercase letters, digits, `-` and `_`. The computer name becomes
the host's name in the spec.

The password is set once in the new system with `chpasswd`. It is never written to the spec.

### Review

The review screen lists every choice. Press its number, or click **Change**, to go back to it.

**Use Secure Boot with your own keys** is off by default. You can turn it on only when the
machine booted with UEFI and its firmware is in setup mode: clear the Secure Boot keys in the
firmware settings, then start the installer again. See [Secure Boot](#secure-boot).

**Erase `<disk>` and install** starts the install.

### Install

The install runs in stages: partitioning, formatting (and encrypting), creating subvolumes,
installing the bootloader, installing the distro, applying your spec, setting your password and
copying your spec. Each command's output streams to the screen. It cannot be interrupted; keep
the machine powered on.

When it finishes, remove the USB stick and pick **Restart now**.

When it fails, the screen names the stage and the error. **Try again** goes back, and
**Open a shell** leaves you at a root shell in the live system. The full log is at
`/var/log/fspec-install.log`.

## Restore from git

Restore installs a machine exactly as an existing spec declares it, instead of writing a new one.

1. On the welcome screen, pick **Restore from git**.
2. Choose the language, keyboard and time zone.
3. Enter the address of your spec repository, and how to sign in:

   | Sign-in method | For |
   | --- | --- |
   | Nothing | A public repository |
   | SSH key | A private key file, for an `ssh` or `git@` address |
   | Access token | An HTTPS address, with a personal access token |

   The token is sent as a header for this one clone and never stored. Neither the key nor the
   token is written to the new system.

4. Choose the host to install. The list shows every host in the spec. Hosts this ISO cannot
   install, such as a macOS host or one with no `system`, are shown with the reason.
5. Choose the disk, as for a new system.
6. Set the password for the host's first declared user.
7. Review and install.

The host is installed with the distro, packages, users and files its declaration names. The host
must declare at least one user.

## Secure Boot

The ISO boots with Secure Boot on. The installed system does not use Secure Boot unless you set it
up: systemd-boot and the kernel images are unsigned.

The review screen's Secure Boot option is designed to enrol your own keys with `sbctl`, keeping
Microsoft's UEFI certificate, sign systemd-boot and the kernels of distros without a signed
kernel (Arch, CachyOS, Void, AerynOS) with your key, and keep distros with a signed shim (Fedora,
Ubuntu, Debian, openSUSE, Universal Blue) booting through it. In this version the install records
the choice but does not run `sbctl` yet. Measured boot and anti-cheat behind a chainloaded shim
are not verified.

## What ends up on disk

| Where | What |
| --- | --- |
| The EFI partition, mounted at `/efi` | systemd-boot and the distro's kernels, or GRUB on a BIOS machine |
| `@<distro>` | The system root, such as `@fedora` |
| `@home` | `/home` |
| `@swap` | The swapfile |
| `@snapshots` | Space for read-only snapshots |
| `@fspec` | FormalSpec's store, mounted at `/opt/fspec` |
| `~/spec` | Your spec repository |

On a machine that booted without UEFI, the installer uses the
[BIOS layout](switching-distros.md#bios) instead.

`~/spec` is a git repository with one commit, "initial spec from the installer":

```text
~/spec/
  spec.star           the host
  modules/            the preset's modules, such as minimal.star and desktop.star
  files/              files the modules use, such as flathub.flatpakrepo
  .gitignore
```

A fresh `spec.star` looks like this:

```starlark
# This machine's spec. `fspec plan` shows what a change would do, `fspec apply` makes it so.
load("//modules/desktop.star", "modules", "system")

hosts = [
    host(
        name = "desktop",
        platform = "x86_64-linux",
        locale = "en_US.UTF-8",
        keymap = "us",
        timezone = "Europe/Madrid",
        system = system("fedora", desktop = "gnome"),
        modules = modules("fedora", desktop = "gnome"),
        users = [user("alex", uid = 1000, groups = ["wheel"])],
        protect = ["/home"],
    ),
]
```

`system(...)` and `modules(...)` come from the preset's module. On Debian and Ubuntu the admin
group is `sudo` instead of `wheel`.

## After the first login

1. Check that the machine matches its spec:

   ```sh
   cd ~/spec
   fspec plan
   ```

2. Lock versions. The installer does not write `fspec.lock`:

   ```sh
   fspec update
   git add fspec.lock
   git commit -m "lock"
   ```

3. Apply. The user scope applies for the first time here:

   ```sh
   fspec apply
   ```

4. Push the repository somewhere you keep it, so you can [restore](#restore-from-git) the machine
   or [manage it from another one](remote-hosts.md):

   ```sh
   git remote add origin git@github.com:alex/spec.git
   git push -u origin main
   ```

5. Put the store on your `PATH`, for example in `~/.bashrc`:

   ```sh
   export PATH="/opt/fspec/current/bin:$PATH"
   ```

From here, [Your first spec](../first-spec.md) and the guides apply as on any machine.

## Limits

- Image distros (Bazzite, Fedora Atomic, Aurora) are listed but stop with
  `<distro> is a bootc image host; the installer cannot install those yet`.
- CachyOS, Void Linux (musl), openSUSE Leap, Aeon and Kalpa are listed, but the spec the
  installer writes for them uses builtins this fspec does not have yet. openSUSE Tumbleweed has no
  bootstrapper in `fspec disk init` yet.
- A fresh install sets `locale`, `keymap` and `timezone` on the host, which this fspec does not
  accept yet, so a fresh install stops when it evaluates the spec. Restore from git is not
  affected.
- Restore installs hosts that declare `fedora()`, `arch()`, `debian()`, `ubuntu()`, `void()` or
  `aerynos()`.

## Try it without a machine

`fspec-install --demo` runs the installer against a made-up machine and writes nothing anywhere.
Use it to see the screens before you install. Without `--demo`, it refuses to start unless it
runs as root, since it partitions disks.

## Troubleshooting

**`No disks found`**

The installer lists whole disks of 16 GB or more. Connect one, then press `r` to look again.

**`Unable to clone. Check the URL and how you signed in, then try again.`**

The clone failed. Check the address, the network (`nmtui` in a shell), and that the key or token
can read the repository.

**`This spec declares no hosts this installer can install.`**

Every host in the spec is for another platform, or declares no `system`. The ISO installs
`x86_64-linux` hosts.

**`host(<name>)(): unknown field `locale`, ...`**

A fresh install wrote a host field this fspec does not read yet. See [Limits](#limits).

**`fedora needs --release <number>, the Fedora release to install`**

The installer reads the current Fedora release from Fedora's release list. It needs the network.

**The install stopped during a stage.**

Open `/var/log/fspec-install.log`. It holds every command the install ran and its output.
