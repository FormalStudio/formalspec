# FormalShell VM rig

Replaces FormalShell's `nix/testvm.nix` and `dev/vm.sh`. Four openSUSE Tumbleweed guests on the
g815, one per slot, declared in the owner's spec as hosts `fstestvm0` to `fstestvm3`
(`hosts/fstestvm/host.star` and its `rootfs/` in FormalSnake/dotfiles) and kept current by
`fspec apply` like any host. Each one has a display nobody looks at directly: it boots the real
hosts' chain on it (Limine, the FormalOS splash, the FormalShell greeter, a desktop), and fspec
records it as PNG frames and types into it. See [VM hosts](content/guides/vm-hosts.md) for how
`fspec vm` works.

## Commands

Run from the macbook, with the spec checkout as the current directory or `--spec`.

| Command | Does |
| --- | --- |
| `fspec vm start fstestvm<n>` | Boots slot n on the g815 and returns once its sshd answers. A running VM is left alone (exit 0). The first start downloads the cloud image, creates the disk and waits for its first-boot setup (about 3 minutes) |
| `fspec vm stop fstestvm<n>` | ACPI power-off, qemu killed after 120 s if the guest hangs. A resumed slot is stopped outright and its overlay deleted |
| `fspec vm save fstestvm<n>` | Waits for the running slot's boot to settle, saves its machine to `ready.state` (about 170 MB, 18 s) and stops it |
| `fspec vm resume fstestvm<n>` | Runs the slot from its save on a throwaway overlay in `/tmp`, in about 1.5 s |
| `fspec vm status` | `running`, `resumed`, `saved`, `stopped` or `absent` for every slot |
| `fspec vm ssh fstestvm<n> [-- cmd]` | `ssh fstestvm<n> cmd` |
| `fspec vm record fstestvm<n> --boot [--seconds 120] [--every-ms 100]` | With the VM off: powers it on paused, takes frame 0, then lets the firmware run, recording in the background for `--seconds`. Without `--boot`, records a running VM. Returns once the guest's sshd answers |
| `fspec vm frames fstestvm<n> <dir>` | Waits for the recording to end, then copies it into `<dir>/frames/000000.png`... |
| `fspec vm type fstestvm<n> '<text>' [--enter]` | Types on the VM's keyboard through a US layout |
| `fspec vm key fstestvm<n> <combo>...` | Presses QEMU qcodes in turn: `ret`, `tab`, `shift-tab`, `ctrl-alt-f2` |
| `fspec apply --host fstestvm<n>` | Converges the guest to the spec |

| Slot | ssh alias | Forward on the g815 | Disk on the g815 |
| --- | --- | --- | --- |
| 0 | `fstestvm0` | `127.0.0.1:2222` | `~kyandesutter/.local/share/fspec/vm/fstestvm0/` |
| 1 | `fstestvm1` | `127.0.0.1:2223` | `~kyandesutter/.local/share/fspec/vm/fstestvm1/` |
| 2 | `fstestvm2` | `127.0.0.1:2224` | `~kyandesutter/.local/share/fspec/vm/fstestvm2/` |
| 3 | `fstestvm3` | `127.0.0.1:2225` | `~kyandesutter/.local/share/fspec/vm/fstestvm3/` |

Each guest: 4 CPUs, 3 GiB (a leg's session holds about 550 MB), qemu at `CPUWeight=20` with a
free-page-reporting balloon, KSM on the g815 merging what the four share, a 40 GiB sparse disk, a `virtio-vga` display (1280x800 at the
greeter), user-mode networking (outbound works, nothing inbound but the ssh forward). The
kernel's console log is `serial.log` in its directory, the last recording `frames/`. To start a
slot over from the image: `fspec vm stop`, `rm -rf` its directory on the g815, `fspec vm start`,
`fspec apply`.

The macbook's `~/.ssh/config` carries the aliases (`ProxyJump g815`, the slot's port, user `kyan`,
agent forwarding, no host key check since a new disk is a new key); it lands with the macbook's
next `fspec apply`. From the g815 itself, `ssh -p 2222 kyan@127.0.0.1`.

## A run per slot

A run starts from the slot's save, never from a cold boot, and every run gets the same clean
machine:

```sh
fspec vm resume fstestvm2        # about 1.5 s, the clock set to now
ssh fstestvm2 ...                # sync the result, run the leg
fspec vm stop fstestvm2          # the overlay and all the leg wrote are gone; the save stays
```

The four slots resume and run side by side, like the g815's eight rig slots, so `just smoke-all`
can take a free VM slot per `vm_only` leg the way it takes a free rig slot per run. Nothing about
the slot has to be reset between legs.

The save goes stale when an apply changes the disk. After `fspec apply --host fstestvm<n>`:
`fspec vm stop`, `fspec vm start` (a cold boot, which drops the old save), `fspec vm save`. A
fresh disk's first apply starts greetd and the sway parent only on the next boot, so a new slot
is start, apply, stop, start, save.

## Watching a boot

```sh
fspec vm stop fstestvm0
fspec vm record fstestvm0 --boot --seconds 90
fspec vm type fstestvm0 formalshell-test --enter   # once the greeter shows, about 20 s in
fspec vm frames fstestvm0 ./artifacts/vm-boot      # after the 90 s
```

Frame n is taken n x 100 ms after power-on, so the file number is the timeline. What the frames
show, in order:

1. OVMF's logo.
2. Limine's FormalOS menu, for its 4 s timeout. The first boot after an apply that installed it
   comes through Limine on a one-time `BootNext`, and formalos-boot puts it first in the firmware
   order once it has booted the machine, exactly as on the real hosts.
3. systemd-boot (no menu, `timeout 0`) starting the `tumbleweed-<kernel>` entry that
   fspec-esp-sync wrote, which counts three tries like a real host's.
4. The FormalOS plymouth splash, from the initrd, on the virtio-gpu card.
5. greetd's FormalShell greeter on sway in software (`/etc/greetd/formalshell-session.sh`).
6. After the password, `/etc/fstestvm/desktop.sh`: Hyprland in software on the same card,
   started as the real hosts start it (`uwsm start`, `start-hyprland`), running `formalshell`
   with the default settings. `SUPER+Return` opens foot, `SUPER+Space`
   the launcher, `SUPER+SHIFT+E` logs out.

## Watching the way out

A recording runs through a reboot (qemu stays up) and ends by itself when qemu exits, so a
shutdown is recorded to the last frame before power-off. Drive the session from its own keys:

```sh
fspec vm record fstestvm0 --seconds 120             # a logged-in VM
fspec vm key fstestvm0 meta_l-shift-e               # log out: Hyprland exits, the greeter returns
fspec vm type fstestvm0 formalshell-test --enter    # log back in
fspec vm key fstestvm0 meta_l-spc                   # the launcher
fspec vm type fstestvm0 shutdown                    # or reboot
fspec vm key fstestvm0 ret                          # arms the row
fspec vm key fstestvm0 ret                          # runs it
fspec vm frames fstestvm0 ./artifacts/vm-shutdown   # returns once qemu has exited
```

A shutdown ends: the desktop, a few frames of QEMU's "Display output is not active" (the guest
has turned the scanout off; real hardware shows black), then the FormalOS shutdown splash until
the last frame. A frame of tty1 in between is the VT showing through, and it is the real hosts'
too: their greetd runs the same way, on vt 1.

The boot chain is the real hosts' own: `modules/boot/boot.star`'s `formalos_boot` with the chime
left out (the VM has no sound card, and the chime waits for one before the greeter), and the
same kernel options as the g815 and e1504g (`quiet splash loglevel=3 udev.log_level=3`, on top of
the image's console and SELinux options, and `plymouth.ignore-serial-consoles`: the serial console
that feeds `serial.log` would otherwise force plymouth's text splash). The guest's keymap is `us`, the layout `fspec vm type`
types through; the real hosts' greeter uses `es`.

## Users

- `kyan`, uid 1000, groups wheel, video, render, input, audio. Key login only (the macbook's and
  the g815's `kyandesutter` keys), passwordless sudo, lingering, so its user manager and the
  sway parent run without a login.
- Its password is `formalshell-test`, for PAM only (`formalshell-lock`, polkit, the greeter):
  sshd takes no passwords.
- `greeter`, the greetd account the FormalShell greeter runs as.

## What is up

System services: `sshd`, `NetworkManager`, `bluetooth` (the `bluetooth` module is loaded so
bluetoothd starts; there is no controller, the honest no-adapter state), `upower` (AC only),
`power-profiles-daemon`, `polkit` (D-Bus activated), `greetd` (boots to `graphical.target`),
`fstestvm-hostapd-wlan1` (`FORMALTEST`, WPA2-PSK-SHA256, password `formaltest-psk`),
`fstestvm-hostapd-wlan2` (`FORMALTEST-EAP`, PEAP/MSCHAPv2, identity `formaltest`, password
`formaltest-eap-pw`), `fstestvm-ap-addrs` (10.90.1.1 and 10.90.2.1) and `fstestvm-dhcp`
(dnsmasq, DHCP only, on the two APs), `fstestvm-modules` (loads the modules below on the first
apply too, before any reboot) and `fstestvm-seatd` (`seatd -g video`: an ssh login holds no logind
seat, so the smoke rig's Hyprland opens the vkms card through it; the greeter and the desktop set
`LIBSEAT_BACKEND=logind` and stay on seat0). geoclue2 is installed and D-Bus activated.

Kernel: `kernel-default` with `mac80211_hwsim` (three radios: wlan0 is NetworkManager's station,
wlan1 and wlan2 the APs, unmanaged) and `bluetooth` loaded at boot. `vkms` is loaded by
`fstestvm-vkms` once the boot splash has quit and unloaded before the shutdown splash starts:
plymouth draws on every DRM card and, with vkms beside virtio-gpu, put its splash off centre. `v4l2loopback` is
installed and configured (`video_nr=10,11`, "Loop Colour" and "Loop IR", `exclusive_caps=1,1`) but
not loaded, so `--mirror` loads it itself as before. Two DRM cards: vkms for the smoke rig and
virtio-gpu for the display; both scripts above pick virtio-gpu by its driver. On kernel 7.3 vkms
sits on the faux bus (`card*/device/driver` is `faux_driver`), so it is found by its device path
`/sys/devices/faux/vkms`, as `dev/smoke.sh`'s `vkms_card_device` already does. Checked by hand:
`AQ_DRM_DEVICES=<the vkms card> dbus-run-session -- Hyprland` over ssh brings up `Virtual-1`
1024x768 beside a logged-in desktop.

User session (`kyan`'s lingering manager): `fstestvm-parent.service`, a headless sway on the
pixman renderer that imports `WAYLAND_DISPLAY` into the user environment, where `dev/smoke.sh`
already looks. pipewire, pipewire-pulse and wireplumber run from the distro's user presets, with a
null sink `virtual-sink` from `/etc/pipewire/pipewire.conf.d/10-virtual-sink.conf`.

Suspend: `systemd-suspend.service` only writes the millisecond stamp to
`/run/formalshell-suspended-at` and sleeps 2 s, as the testvm did for `--sleep`.

Greeter: it appends its log to `/tmp/formalshell-greeter-session.log` and writes
`/tmp/formalshell-greeter-post-auth.png` with grim after it exits, the paths
`dev/smoke-greeter.sh` reads. Its settings are the spec's
(`hosts/fstestvm/rootfs/var/lib/formalshell-greeter/.config/formalshell/settings.json`): one
session, the desktop above. There is no `greeter-cartridge` specialisation, Tumbleweed has none:
for `--preset cartridge` add the theme to that file in the spec, or edit the guest's copy and
`sudo systemctl restart greetd` until the next apply puts it back.

Tools on PATH, by Tumbleweed package: hyprland, sway, foot, grim, slurp, wtype, wl-clipboard,
wlrctl, wf-recorder, wlsunset, brightnessctl, ddcutil, libnotify-tools, dbus-1-daemon
(`dbus-run-session`), mpv with mpv-mpris, ffmpeg-8, cava, qrencode, ImageMagick, tesseract-ocr,
python313 with gobject and evdev, evolution-data-server, podman, jq, just, rsync, strace, socat,
v4l-utils, pulseaudio-utils (`pactl`), pipewire-tools, iw, hostapd, git, curl. Fonts: JetBrains
Mono, Noto Color Emoji, Adwaita icons; Geist comes from the FormalShell package's own fonts.
localsend has no Tumbleweed package; the tarball's `localsend-cli` is the one the legs use.

## Getting a FormalShell build in

The spec installs the fspec `formalshell` package (the release tarball of the rev pinned in
`modules/linux/store.star`, the same one g815 and e1504g run) under `/opt/fspec/current`, so
`/opt/fspec/current/bin/formalshell{,-ipc,-greeter}` and the helpers in its
`lib/formalshell/bin` are always there; the greeter and the desktop run those. When CI has not
built a rev yet, the apply builds it on the g815 over the agent it forwards. For a build of a
working tree:

1. On the g815, `dev/build.sh` (or `just build` from the mac) leaves `./result` laid out as the
   tarball unpacks.
2. Copy it in: `rsync -a --delete result/ fstestvm0:formalshell-result/` (from the g815, use
   `-e 'ssh -p 2222'` and `kyan@127.0.0.1:`).
3. Run with `FS_RESULT=$HOME/formalshell-result dev/smoke.sh <flags>` inside the guest, after
   syncing the tree the same way.

A release tarball from `dev/tarball.sh` works the same: unpack it in the guest and point
`FS_RESULT` at it. Nothing compiles in the guest.

## What is left for the FormalShell side

- `dev/smoke.sh`'s VM mode: its vkms branch applies unchanged (the sway parent has no render
  node, and seatd is up). The virtio-gpu card is a second card it must not take.
- `dev/vm.sh` and `dev/vm-lock.sh` call `fspec vm resume fstestvm<slot>`, `ssh fstestvm<slot>`
  and `fspec vm stop fstestvm<slot>` instead of booting a NixOS VM. Slot locking stays
  FormalShell's: a mac-side flock per slot, as `FS_G815_SLOTS` does for the rig slots, over
  `fstestvm0` to `fstestvm3`.
- `just check fs-auth|fs-audio|fs-bluez|fs-network|fs-upower` run `cargo test` in the guest
  against its real daemons; the guest has no Rust toolchain, so build the test binaries on the
  g815 (`cargo test --no-run`) and copy them in.
- A boot leg (`dev/smoke-boot.sh` or similar) can drive the sequence under "Watching a boot" and
  read the frames: `fspec vm record --boot`, `fspec vm type`, `fspec vm frames`.
