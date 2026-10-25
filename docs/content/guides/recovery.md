+++
title = "Recovery"
description = "Boot back into NixOS after the in-place migration, undo the migration, roll back a bad apply or a bad image, and what fspec forget nixos cannot undo."
+++

Pick the section that matches what broke. The first three need a machine that still boots or a
boot menu; the undo needs the [installer ISO](installer.md) or the `fspec-live` entry.

## Boot back into NixOS

After the [in-place migration](switching-distros.md#migrate-a-nixos-ext4-machine-in-place), NixOS
lives in the `@nixos` subvolume and keeps its own boot entries on the ESP. A root installed
beside it, such as Bazzite, has entries of its own.

1. Reboot and open the systemd-boot menu (hold or tap `Space` at power-on).
2. Pick the NixOS entry. The newest generation is the first one.
3. Log in. Nothing about the other root changed.

The first boot of the migrated NixOS generation has three tries. After three failed boots,
systemd-boot stops picking it and falls through to the `fspec-live` entry. Once NixOS marks a
boot good, the trial options are dropped; `sudo fspec disk unmask` drops them by hand.

NixOS generations older than the migration cannot boot btrfs, so their entries sit in
`loader/entries.ext4/` on the ESP, out of the menu. Windows keeps its own boot entry on the same
ESP and is not touched.

If no entry boots, start the installer ISO or the `fspec-live` entry. Mount the filesystem with
`subvolid=5` to reach `@nixos` and `@home`:

```sh
sudo mount -o subvolid=5 /dev/nvme0n1p2 /mnt
```

## Undo the migration

The migration converted the ext4 root with `btrfs-convert --uuid copy` and kept the original
ext4 image as `ext2_saved`. `btrfs-convert -r` puts that image back, so the partition is ext4
again with NixOS as it was at conversion.

Before you start, know what this costs:

- Everything written to the filesystem since the conversion is gone, including changes in
  `@home` and `@nixos`. Copy what you need to another disk first.
- Every root on that btrfs filesystem is gone with it, Bazzite included.
- It only works while `ext2_saved` exists. The cleanup that `fspec disk plan` lists deletes it.
  Once it is deleted there is no way back to ext4.
- The partition must be unmounted, so run it from the ISO or the `fspec-live` entry.

Steps:

1. Boot the installer ISO or the `fspec-live` entry.
2. Check that `ext2_saved` is still there and nothing is mounted (the examples use the
   e1504g's root, `/dev/nvme0n1p2`, and its ESP, `/dev/nvme0n1p1`; the g815's root is
   `/dev/nvme0n1p5`):

   ```sh
   sudo mount -o ro,subvolid=5 /dev/nvme0n1p2 /mnt
   ls /mnt | grep ext2_saved
   sudo umount /mnt
   ```

3. Roll the conversion back:

   ```sh
   sudo btrfs-convert -r /dev/nvme0n1p2
   ```

4. Restore the NixOS boot entries. Mount the ext4 root and the ESP, then move the old
   generations back:

   ```sh
   sudo mount /dev/nvme0n1p2 /mnt
   sudo mount /dev/nvme0n1p1 /mnt/boot
   sudo mv /mnt/boot/loader/entries.ext4/*.conf /mnt/boot/loader/entries/
   ```

   Then delete the entry of the btrfs generation, which cannot boot ext4: the newest
   `nixos-*.conf` in `loader/entries/`, the one with `+3` in its name.
5. Reboot and pick an older NixOS generation. From there, `nixos-rebuild boot` rewrites the
   entries for your own configuration.

If the migration itself failed, you do not need this: it rolls itself back, and the error ends
with `rolled back: <device> is ext4 again, with NixOS as it was`. A staged migration
(`disk migrate --stage`) writes its output to `fspec/migrate.log` on the ESP (`/boot` on NixOS),
reboots into NixOS on ext4 after a rollback, and after a reset rolls back by itself on the next
boot. Only when it cannot finish the rollback does it stay in the live system, reachable over
ssh. After a killed run by hand, boot `fspec-live` and run `fspec disk migrate <device>` again,
which replays the journal on the ESP (see [How a failed NixOS migration rolls back](switching-distros.md#how-a-failed-nixos-migration-rolls-back)).

## Roll back a bad apply

An apply that fails part way rolls itself back. For one that succeeded and turned out wrong:

```sh
fspec rollback
```

That goes back one generation in each scope. `fspec rollback 12` goes to generation 12, and
`--scope system` or `--scope user` limits it to one scope. `fspec rollback 0` removes everything
fspec installed. It does not touch `fspec.lock` or the spec, so revert those in git too.
[Removal and rollback](removal-and-rollback.md) has the details.

## Roll back a bad image

On an [image host](system-layer.md#image-hosts) a new image is a new deployment, and the old one
stays on disk.

1. In the boot menu, pick the previous deployment. This works even when the new one does not
   reach a login.
2. To make that choice stick, run:

   ```sh
   sudo bootc rollback
   ```

3. Reboot.
4. Put the lock back so the next `fspec apply` does not stage the bad image again:

   ```sh
   git revert HEAD
   ```

   (Use the commit that bumped the digest in `fspec.lock`.)

## fspec forget nixos is one way

```sh
sudo fspec forget nixos
```

This deletes the `@nixos` subvolume, `EFI/nixos` and the `nixos-*.conf` entries on the ESP. It
refuses the root you booted, and refuses unless the running root owns boot
(`fspec switch --adopt-boot`).

Nothing restores it. After it runs, the NixOS boot menu entries are gone, and the only route back
to the old system is `btrfs-convert -r` from the section above, which needs `ext2_saved` and
loses everything written since. Run `forget nixos` only after you have stopped needing NixOS, and
delete `ext2_saved` last.
