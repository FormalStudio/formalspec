# FormalShell VM rig: making a run start in seconds

Proposals for the VM rig in [formalshell-vm-rig.md](formalshell-vm-rig.md), measured on the
g815 on 2026-10-29 while `fstestvm0` was up. Each item names what it costs and what it buys,
biggest first.

Landed: 1 as a save per slot (`fspec vm save`, `fspec vm resume`) rather than one shared base,
since each slot is its own spec host with its own disk; 2 as four slots of 4 CPUs and 3 GiB with
KSM and a balloon; 5's tmpfs overlay and `CPUWeight=20`. 3 and 4 stand as proposed.

## Where the time goes today

| Stage | Time | Source |
| --- | --- | --- |
| First `fspec vm start` (cloud image, new disk) | about 3 min | formalshell-vm-rig.md |
| qemu start to the login prompt, existing disk | 18 s | `pid` mtime 11:54:05, last `serial.log` write 11:54:23 |
| `fspec apply --host fstestvm<n>` on a current guest | the plan alone, seconds | |
| Copying a build in (`rsync result/`) | 3 s or less | the nested rig's tree sync is 2.6 to 4.5 s |
| A leg itself | 30 to 120 s | the nested rig's `--menu` session is 56 s of a 70 s run |

The boot is the only stage the VM adds on top of the nested rig. A leg then costs what it
costs on the nested rig, so after the boot, the next lever is running more legs at once.

## 1. Start from a saved, booted state (18 s to about 2 s)

Boot each slot once, let `fstestvm-parent` (sway) come up, then save the whole machine:

1. QMP `stop`, then `migrate` to `exec:zstd -1 > ready.state` (RAM plus device state, about the
   guest's used memory, 1 to 2 GiB compressed for an idle guest).
2. With the guest still stopped, make `disk.qcow2` the read-only base it was at that moment.
3. Per run: `qemu-img create -f qcow2 -b disk.qcow2 -F qcow2 run.qcow2`, then qemu with the same
   device list, `-drive file=run.qcow2` and `-incoming "exec:zstd -dc ready.state"`, then QMP
   `cont`.

The restored guest is at the sway prompt with sshd listening. A run that ends deletes
`run.qcow2`, so every leg starts on the same clean disk and nothing a leg did (a NetworkManager
profile, a polkit rule, a stuck greeter) carries into the next. `fspec apply` and package updates
go to the base, followed by a new save; `fspec vm start` can compare the spec generation recorded
next to `ready.state` and re-save when it moved.

What to handle:

- The guest clock is the save's. Run `chronyc makestep` (or `hwclock --hctosys`) over ssh right
  after `cont`, before a leg reads the time (`--theme-auto` compares against the wall clock).
- The device list and the machine type must be identical between save and restore; build both
  command lines from the same function in `crates/fspec-try/src/vm.rs`.
- A save taken with a vhost-user device (virtiofs, below) attached will not migrate; leave it out
  of the saved machine.

## 2. More VM slots off one base (two to four)

With per-run overlays the slots share one base image, so a slot costs RAM and CPU, not disk.
Today each guest takes 6 CPUs and 8 GiB; a smoke session in the guest is about 550 MB on the
nested rig. Suggested: 4 slots at 4 CPUs and 3 GiB each (12 GiB, against the 20 GiB the g815
has free with the owner's desktop up), `fstestvm0` to `fstestvm3`, ssh ports 2222 to 2225.
Two extra settings make that cheap:

- KSM on the host (`/sys/kernel/mm/ksm/run = 1`, qemu's `mem-merge` is on by default): four
  guests restored from the same state share most of their pages.
- `-device virtio-balloon,free-page-reporting=on`, so a guest hands back memory a finished
  leg freed.

FormalShell's `dev/remote.sh` already picks among N slots by flock; the VM side takes the same
picker with `FS_VM_SLOTS`.

## 3. The build: keep rsync, skip virtiofs for now

Copying `result/` and the tree into a guest is 3 s at most, against the 18 s boot it rides on.
virtiofs (`virtiofsd` plus `memory-backend-memfd,share=on`) would save those seconds, but a
vhost-user device blocks the migration item 1 depends on, and hot-plugging it after every
restore buys back less than it costs. Revisit if the copy grows past 10 s.

## 4. The GPU: not for these legs

The VM legs (`--wifi`, `--polkit`, `--power`, `--sleep`, `--mirror`, `--device-routes`,
`--panel-morph`, the greeter) need real system daemons, not rendering speed, and `dev/smoke.sh`'s
VM mode is the vkms software card by design. virgl (`virtio-vga-gl`) would change the render path
those legs were written against. The RTX 5070 stays with the owner's desktop and games.

## 5. Small ones

- `cache=none,aio=io_uring` is already set. Put each run's overlay on `/tmp` (tmpfs on the g815)
  so a leg's writes never reach the NVMe.
- Run each slot's qemu in a slice with `CPUWeight=20`, the same weight the nested rig's slot users
  run at (`hosts/g815/rig.star`), so a VM slot yields to the desktop.
- Pull artifacts from the guest straight to the macbook through the `ProxyJump` alias rather than
  via the g815's disk.
