# Handoff, 2026-10-25

Work stopped on purpose, because weekly usage ran low. origin/main is at `2cf8d2b`, and all pushed work is on main.

## State of the machines

- **g815:** rebooted into Windows. Your test VM `fspec-installer` (qemu:///system) runs Fedora GNOME and works. Open it with:
  `systemd-run --user --collect --unit=fspec-installer-view -E GDK_BACKEND=x11 virt-manager --no-fork --connect qemu:///system --show-domain-console fspec-installer`
  The x11 flag avoids virt-manager drawing wrong under Wayland fractional scaling. Don't run `--fresh` on it.
- **e1504g:** stopped answering ssh (banner timeout). At that point the matrix agent had a docker job running there, most likely an ISO build, on 8 GB of RAM. If it doesn't recover, power-cycle it. Leftovers are in `~/Developer/ecosystem/FormalSpec-wt/matrix`.
- **g815 worktrees:** `~/Developer/ecosystem/FormalSpec-wt/{iso-vm,matrix,migrate,win-test}`. After each ISO build, delete `installer/iso/work/lb` with sudo, because the disk is tight.

## Uncommitted work

`formalspec-wt/matrix-fix` (macbook, branch `matrix-fix`, already merged up to `2cf8d2b`) holds two fixes that haven't been booted in a VM:
- Ubuntu wired network: Ubuntu's NetworkManager only manages Wi-Fi and modems, so the fix adds Ubuntu Desktop's netplan file to make it manage wired devices too (`presets/minimal.star`, `spec/tests.rs`, `docs/design.md`).
- Faster pacstrap: the ISO's pacman downloads ten packages at a time (`installer/iso/config/hooks/normal/0105-pacman.hook.chroot`). This needs an ISO rebuild.

`formalspec-parity` (dirty, `examples/kyan` Discord/moonlight edits) belongs to a peer. It is left untouched.

## Distro matrix (installer, desktop preset)

| Distro | Result |
|---|---|
| Fedora | Pass: greeter, DNS, `fspec plan` clean, locale, keymap, tz. 474s (was 558s) |
| Debian | Pass, 683s |
| Ubuntu | Boots to the greeter. No wired network (fix above, unbooted). The greeter is English because it lacks a language pack |
| Arch, Void | Installed, but the first boot was never checked. Arch takes 764s, of which pacstrap is 337s |
| AerynOS | Not run |

To resume, on the g815 (NixOS) in `FormalSpec-wt/matrix`, run:
`t/matrix.sh ubuntu-desktop arch-desktop void-desktop aerynos-desktop`
It refuses to start a second copy. After that, run the minimal and server presets on two distros.

## v0.1 checklist (end of docs/design.md)

1. Parity: `examples/kyan/GAPS.md` has 3 items that need you (quickshell, macos-vm, g815 ext4 root carve) and 2 that aren't verified (uxplay/colloid, Discord). design.md names Bazzite as the parity target, but the real target is Tumbleweed. Fixing the doc wording is still waiting on your decision.
2. Unattended NixOS migration: proven in a VM.
3. Switch beside, then rollback: proven in a VM.
4. `fspec apply --all` convergence: not started.
5. CI: green. The nightly lock-update workflow runs with the `FSPEC_UPDATE_TOKEN` secret. The release ISO workflow has never run.
6. Recovery guide: done.

## Unproven

- Beside Windows: booting real Windows, the hibernation and BitLocker refusals, Secure Boot, and chkdsk after the shrink. Minor issues: bootctl replaces `EFI/Boot/bootx64.efi`, and `bootctl list` can't see kernels on XBOOTLDR.
- Migration: a bad bootc image falling back, Wi-Fi and tailscale overlays, the real 1 GB ESP with the g815's closure, and the real user `kyandesutter` on the NixOS root.
- `installer/iso/vm.sh` has never been run.

## Cleaned up

Removed 14 merged worktrees and their branches on the macbook. Kept `matrix-fix`, `formalspec-parity`, and the three unmerged `.claude/worktrees/agent-*`.

# Addendum, 2026-10-26: e1504g applies take too long

Two bugs were reported on e1504g at about 09:35. The fixes were written within 30 minutes, and 1h30 later neither was on the host yet. Here is where the time went, and what to fix in fspec.

## Where the time went

- **Every new FormalShell rev compiles cold.** A rev with no OCI digest in `fspec.lock` gets built from source. The g815 builder sets up a fresh rustup toolchain and an empty target dir under `/tmp/.tmp*/out.work`, so one rev costs about 10 minutes. Three `git(...)` bumps landed in an hour (formalshell adefb51e, formalshell 692c3961, formalmusic 2aab94ef), and each one paid that full cost.
- **The builder's output was lost, so e1504g built it again.** g815 finished formalshell adefb51e, and `~/.cache/fspec/build/formalshell-adefb51e1188` was gone on g815 afterwards. The apply on e1504g then ran `dev/tarball.sh` in `docker run debian:bookworm` on e1504g itself, which is the 8 GB host that must never compile. That apply was killed before activation.
- **Applies ran on top of each other, and orphans kept running.** At one point three applies were running on e1504g. Two of them, at `babc860` (started 10:07 and 10:48), had lost their ssh client and kept running with nobody reading their output. Both used `/var/lib/fspec/staging/26`: the 10:48 one recreated it under the 10:07 one, which then built formalmusic a second time. Both builds also ran in the same g815 dir, `~/.cache/fspec/build/formalmusic-2aab94ef66e0`. The 10:07 one was killed at 10:56.
- **The macbook's fspec is too old.** The installed fspec is 0.1.0 (nix store). Its `--dirty` cleanup is `rm -rf ".../<commit>-dirty"*`, which fails under fish (the login shell on e1504g) when nothing matches. main already uses `find`. Applies had to go through `target/debug/fspec`.
- **An apply read the lock while it was being rewritten.** An apply started while `fspec update` was rewriting `fspec.lock` failed with `pkg formalshell is not in fspec.lock`.

## What to change

1. Keep a build cache per package on each builder: the rustup toolchain and `CARGO_TARGET_DIR` stay between revs, so a rev bump is an incremental build (a minute or two) instead of a cold one.
2. When a host declares `builders`, never fall back to building on the host. If no builder output arrives, fail and say which builder and why.
3. Find out why the builder's output didn't arrive: the build dir was deleted, and the host built again.
4. Take a per-host apply lock on the target, so a second apply waits or is refused, and give each builder job its own dir. Stop the remote apply when its ssh client goes away (run it under `systemd-run --scope`, or exit on SIGHUP), so a dropped session can't leave an orphan behind.
5. Let file and service changes apply without waiting for package builds (a flag, or files first and packages after).
6. Rebuild the macbook's fspec from main (it also needs 50ad7f1, the spec lookup order).
7. Write `fspec.lock` atomically (temp file, then rename), so a concurrent read sees either the old lock or the new one.
8. Point 1 is the big one. `crates/fspec-build/src/run.rs:34` builds every job in a fresh `tempfile::tempdir()`, so the toolchain lands in `out.work/rustup` and every crate compiles from scratch each time. The FormalShell 692c3961 build on g815 at 11:30 took about 20 minutes with the load average at 3 to 4 on 24 cores: CPU was not the limit.
9. FormalShell side (`dev/tarball.sh`): lines 135 to 148 run seven `cargo build`s one after another (the shell, ttfx, tensaku, matugen, nothingctl, openscq30, earbuds). Each one ends in a single-threaded tail, so the cores sit idle until the next starts. The third-party builds don't depend on each other and can run concurrently (`&`, then `wait`, failing if any of them failed). About 30 minutes of work.
10. Also `dev/tarball.sh:191`: `nix/earbuds-art.py` shrinks the earbud images on one core and sat at 99% CPU during that build. Cache its output by input hash, or process the images in a `multiprocessing.Pool`.

## The fixes waiting for e1504g

- **T3 Code missing from the launcher, connections and logins gone** (uncommitted, in this tree). On every start the AppImage rewrites `~/.local/share/applications/com.t3tools.T3Code.desktop` as a hidden URL handler (`NoDisplay=true`), with `Exec` set to the bare AppImage, so `T3CODE_HOME` is dropped. The next launch then used `~/.t3` instead of `~/.t3-desktop`, which still holds the connections and logins. Fix: the visible entry becomes `examples/kyan/home/.local/share/applications/t3code.desktop`, and both entries launch `examples/kyan/home/.local/bin/t3code-desktop`. That wrapper sets `T3CODE_HOME` and `T3CODE_DESKTOP_LAUNCHER_PATH`, and upstream's `DesktopLinuxUrlHandler.ts` uses the second one as its `Exec`. `fspec plan` showed exactly these files plus main's own changes. It also restores the stripped-down `~/.claude-pulse/settings.json` on e1504g.
- **KDE apps in the Hyprland launcher** (FormalShell, branch `fix/launcher-showin`, worktree `FormalShell-wt/showin` on the macbook and on g815). `parse_entry` in `crates/formalshell-rs/src/services/appicon.rs` skipped `NoDisplay` and `Hidden` but ignored `OnlyShowIn` and `NotShowIn`. Every `kcm_*` module, systemsettings and the GNOME panels showed up under Hyprland. It now reads them against `XDG_CURRENT_DESKTOP` the way GLib does. Dolphin, Konsole, KWalletManager, System Monitor and Qrca have no `OnlyShowIn`, so they still show. Hiding those would need a spec override. Not tested yet: the g815 devshell build fails because ttfx 0.4.0's `asm_diff` test segfaults. The test is running on the mac VM (`dev/vm-lock.sh just vm-cargo test -q -p formalshell-rs show_in`).
- **Moved at 11:20:** the spec is now the `formalspec` branch of FormalSnake/dotfiles (history kept), checked out at `~/.config/nix-wt/formalspec`, with its own `fspec-packages.yml` (triggers on `formalspec`, cache at `ghcr.io/formalsnake/fspec-cache`) and `fspec-update.yml` (workflow_dispatch only, because schedules run from the default branch alone). formalspec main no longer has `examples/kyan`. The KDE shadow entries are `5c41c26` there. Apply from that worktree.
- **Status at 11:06:** both fixes are pushed. The T3 change is `35d4d3c`. The launcher fix is FormalShell `facc1a86`, bumped in `35164e0`; its logic test passes in a standalone rustc build, and `cargo check -p formalshell-rs` passes on g815. e1504g is still on generation 25. kyandesutter-d3 is running an apply at `50ad7f1`, and one more from main at `35164e0` or later carries both fixes.
