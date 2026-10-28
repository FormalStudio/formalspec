+++
title = "Commands"
description = "Every fspec command and flag, grouped by what you use it for."
+++

`fspec <command> --help` prints the same information from the binary you have installed.

## Global options

These work with every command.

| Option | Default | Meaning |
| --- | --- | --- |
| `-s`, `--spec <path>` | see below | The spec entry point, or a directory holding `spec.star`. The lock and `secrets.star` sit next to it |
| `--root <dir>` | `/` | Treat this directory as `/`. Commands are logged to `<root>/var/log/fspec-commands.log` instead of run, so you can try an apply safely |
| `--home <dir>` | `$HOME` | The user's home (with `--root`, `<root>/home/<user>`) |

Without `--spec`, fspec picks the config in this order:

1. A `spec.star` in the current directory.
2. For `plan` and `update`: the `dev.star` project at or above the current directory.
3. `$FSPEC_SPEC`, a spec file or a directory holding one. Export it to run `fspec apply` for your
   machine's main config from any other directory.
4. `spec.star`, which fails if it is not there.

`--host <name>` appears on most commands. Without it, the command acts on the host whose name
matches this machine.

## Everyday

### fspec update

```
fspec update [<pkg>...] [--cache] [--images] [--registry <r>]
```

Resolves versions, URLs and hashes from upstream and rewrites `fspec.lock`. Prints one line per
change. With package names, re-resolves only those and keeps every other entry and every `gh:`
load at its locked commit. Entries no host declares are dropped. Hosts that declare one package
with their own `asset` glob each lock the asset their glob picks, at one shared `version`. The `fspec` package every Linux host carries is
locked like any other, at the newest commit of FormalSpec's `main` that changes its sources.

| Flag | Meaning |
| --- | --- |
| `--cache` | Only record the OCI digest of each CI-built `git(...)` package ([Building from source](../guides/source-builds.md)), resolving nothing. CI's `record` job runs it |
| `--images` | Pin each image host's pushed image digest instead of resolving packages ([The system layer](../guides/system-layer.md)) |
| `--registry <r>` | Registry for `--images` (default `ghcr.io/<owner>`) or `--cache` (default `ghcr.io/<owner>/fspec-cache`) |

In a directory with a `dev.star` and no `spec.star`, it locks the project environment instead.

### fspec plan

```
fspec plan [--host <h> | --all] [--json] [--dirty]
```

Shows what `apply` would change. Never changes anything.

| Flag | Meaning |
| --- | --- |
| `--host <h>` | Plan this host |
| `--all` | Plan every host, asking each remote host over ssh. An unreachable one prints `unreachable, skipped` |
| `--json` | Print the plan as JSON |
| `--dirty` | Ship a spec with uncommitted changes to remote hosts anyway |

[How FormalSpec works](../concepts/how-it-works.md#plan) explains the output.

### fspec apply

```
fspec apply [--host <h> | --all] [--dirty] [--scope system|user]
```

Converges a host. A remote host is reached over ssh.

| Flag | Meaning |
| --- | --- |
| `--host <h>` | Apply this host |
| `--all` | Apply every remote host in spec order, then this machine. Stops at the first failure |
| `--dirty` | Allow uncommitted changes when applying remote hosts |
| `--scope <s>` | Apply only the `system` or the `user` scope. Without it, the system scope runs through `sudo`, then the user scope |

### fspec rollback

```
fspec rollback [<generation>] [--host <h>] [--scope system|user]
```

Re-applies an earlier generation as a new one, the previous one by default. Packages, files,
services and app data come back. `fspec.lock` is not changed.

### fspec gc

```
fspec gc --older-than <age> [--host <h>] [--scope system|user]
```

Deletes generations, their trash and the store paths no kept generation uses, when last touched
longer ago than `<age>` (`90s`, `30m`, `12h`, `14d`, `2w`). The current generation is always
kept. `--older-than 0s` keeps only what the current generation needs.

## Files and app data

| Command | Does |
| --- | --- |
| `fspec adopt <path> [--host <h>]` | Copies an app's edit of a managed file back into the spec repository. `<path>` is the path as the spec writes it (`~/.config/x`) or on disk |
| `fspec watch` | Reports drift on managed files as it happens |
| `fspec orphans [--host <h>]` | Lists app data under `~/.config`, `~/.local/share`, `~/.local/state` and `~/.cache` that no package claims |
| `fspec orphans --claim <path> <pkg>` | Records `<path>` as `<pkg>`'s data, so removing the package trashes it |
| `fspec orphans --delete <path>` | Moves `<path>` to the trash |

## Secrets

| Command | Does |
| --- | --- |
| `fspec secret edit <file> [-i <identity>]...` | Decrypts a secret into `$EDITOR` and encrypts the result to the recipients in `secrets.star`. A file that does not exist yet starts empty |
| `fspec secret rekey [-i <identity>]...` | Re-encrypts every secret to its current recipients |
| `fspec secret install [--scope system\|user]` | Decrypts the last apply's secrets again from their kept ciphertext, for a boot that starts with an empty `/run` |

`<file>` is the `secrets.star` key, its file name, or the name without `.age`. `-i` is repeatable;
the default identities are `~/.ssh/id_ed25519`, `~/.ssh/id_rsa` and `~/.config/fspec/age.key`.
See [Secrets](../guides/secrets.md).

## Project environments

| Command | Does |
| --- | --- |
| `fspec dev` | Opens `$SHELL` in the environment of the nearest `dev.star` |
| `fspec dev -- <cmd>` | Runs one command in it |
| `fspec dev plan` | Shows what the environment holds and what the lock could not resolve |
| `fspec dev up` / `fspec dev down` | Starts the project's services and leaves them running, or stops them |
| `fspec dev export` | Prints the bash that turns the current environment into the project's |
| `fspec dev direnv` | Prints the direnv library that defines `use fspec` |

All take `--dir <dir>` to use another project directory. See
[Project environments](../guides/dev-environments.md).

## Sync

```
fspec sync [<item>...] [--peer <h>] [--from <h>] [--rollback] [--dry-run] [--watch [<seconds>]]
```

Syncs app data with the other hosts that list the same `sync(...)` item, every item this host
lists by default.

| Flag | Meaning |
| --- | --- |
| `--peer <h>` | Only with this peer |
| `--from <h>` | Copy each item from this host over the other's copy. The handoff of a `seed` item |
| `--rollback` | Put the items' previous version back on this host |
| `--dry-run` | Print what would move, change nothing |
| `--watch [<seconds>]` | Sync again every 60 seconds, or the given interval, until stopped |

See [Sync between hosts](../guides/sync.md).

## Builds and CI

| Command | Does |
| --- | --- |
| `fspec build --pkg <p> --platform <t> --out <dir> [--host <h>]` | Builds a `git(...)` package at its locked commit into `<dir>/<p>-<t>.tar.zst`. `<t>` must be this machine's platform. With `--host`, tries that host's builders first |
| `fspec ci packages` | Writes `.github/workflows/fspec-packages.yml`, which builds every `git(...)` package per platform and pushes it to the OCI cache |
| `fspec ci init [--registry <r>]` | Writes `.github/workflows/fspec-images.yml`, which builds and pushes every image host's bootc image |
| `fspec image containerfile [--host <h>]` | Prints the Containerfile CI builds for an image host |

## Distros and disks

These change how the machine boots. Read [Switching distros](../guides/switching-distros.md)
before you run any of them. All need root.

| Command | Does |
| --- | --- |
| `fspec switch --distro <d> --host <h> [--release <r>]` | Bootstraps the distro the host declares into a new btrfs root beside this one. `<d>` is `fedora`, `arch`, `debian`, `ubuntu`, `void` or `aerynos`. `--release` is the Fedora release or Debian/Ubuntu suite |
| `fspec switch --beside <name> [--inherit @<root>] [--device <part>]` | Installs the host's `tumbleweed()` into `@<name>`, beside the other roots |
| `fspec switch --to-image --yes` | Converts this classic system to the host's bootc image. Reinitialises `/boot` |
| `fspec switch --to-image --beside <name> [--inherit @<root>] [--pull-from <repo>]` | Installs the host's bootc image into `@<name>` beside this root |
| `fspec switch --adopt-boot` | Makes the running root own boot, so the old one can be forgotten |
| `fspec switch --reset-var` | Stages a reset of `/etc` and `/var` to the image's defaults for the next boot of a bootc host |
| `fspec forget <distro>` | Deletes a root made by `switch`, or the NixOS a migration kept, and its boot entries |
| `fspec disk plan [<device>] [--nixos-system <path>]` | Prints the steps that bring a disk onto a supported layout |
| `fspec disk migrate [<device>] [--backup <path> \| --no-backup] [--swap <GiB>] [--nixos-system <path>]` | Runs the in-place steps of `disk plan` |
| `fspec disk migrate --stage <iso> --nixos-system <path> [--overlay <tar>] [--backup <path> \| --no-backup]` | From the running NixOS root on ext4, stages the live ISO to boot once from the disk and migrate this root unattended |
| `fspec disk init <device> [--layout default\|zfs\|bios] [--luks] [--distro <d> --host <h>] [--release <r>] [--backup <path>] [--swap <GiB>]` | Partitions and formats a disk from scratch |
| `fspec disk unmask [--esp <dir>]` | After the first good boot of a migrated NixOS, drops the trial boot options |

## Trying distros

None of these change the host. See [Trying distros](../guides/trying-distros.md).

| Command | Does |
| --- | --- |
| `fspec try <distro\|url\|path> [--host <h>] [--keep <name>] [--cpus <n>] [--memory <GiB>] [--disk <GiB>] [--headless] [--print]` | Boots the ISO in a KVM VM with a fresh disk, deleted when the VM stops. `<distro>` is `tumbleweed`, `fedora`, `ubuntu`, `debian`, `arch`, `nixos`, `aerynos` or `void`. `--host` runs it on that host over ssh and opens its display here over VNC. `--keep` keeps the disk under `<name>` |
| `fspec try --keep <name>` | Boots a kept VM again from its disk |
| `fspec try --list [--host <h>]` | Lists the distros, and the kept VMs, running VMs and cached ISOs on the host |
| `fspec try --delete <name> [--host <h>]` | Deletes a kept VM |

## Diagnostics

| Command | Does |
| --- | --- |
| `fspec facts [--json]` | Prints this machine's host name, platform and distro, as `apply` compares them with the declaration |
| `fspec --version` | Prints the version |
