+++
title = "Files and paths"
description = "Every directory fspec reads or writes on a host, what lives there and which scope owns it."
+++

FormalSpec writes to a small, fixed set of places. Everything else on the machine is either
declared in your spec or left alone.

## Your repository

| Path | Meaning |
| --- | --- |
| `spec.star` | The entry point. Sets `hosts` |
| `fspec.lock` | Every pinned version, written by `fspec update` |
| `secrets.star` | Recipients of each `.age` file |
| `dev.star` | A project environment, in any repository |
| `.fspec/loads/` | `gh:` load files shipped with a remote apply |

## Store and generations

| Path | Meaning |
| --- | --- |
| `/opt/fspec/store/<hash>-<name>-<version>/` | One unpacked package. Never rewritten; reused when present |
| `/opt/fspec/generations/<n>/bin/` | A generation: symlinks into the store |
| `/opt/fspec/current` | Symlink to the active generation |
| `/opt/fspec/current/bin` | Put this on `PATH` |
| `/opt/fspec/current/pkgs/<name>` | A package's whole tree, for absolute paths to libraries and data |
| `/usr/local/share/<dest>` | Links made by `pkg(share = ...)` (Linux) |
| `/etc/<dest>` | Links made by `pkg(etc = ...)` (Linux) |
| `/Applications/<App>.app` | Copies made by `pkg(apps = ...)` (macOS; a clone on APFS) |
| `/var/cache/fspec/pkgs/` | Cached distro package files the lock pins |
| `/var/cache/fspec/keys/` | Repository signing keys for `repo(...)` |

The same `/opt/fspec` root is used on macOS.

## State

| Path | Scope |
| --- | --- |
| `/var/lib/fspec/state.json` | System: what FormalSpec owns machine-wide |
| `~/.local/state/fspec/state.json` | User: what it owns in your home |
| `<state dir>/generations/<n>.json` | A snapshot of state per generation, used by `rollback` |
| `<state dir>/journal.json` | The undo journal of an apply in progress |
| `<state dir>/secrets/<sha256>.age` | Kept ciphertext of each applied secret, for `fspec secret install` |

Deleting a state file makes FormalSpec forget what it owns. The next apply then treats every
resource as new, and removals stop working for anything it forgot.

## Trash

| Path | Holds |
| --- | --- |
| `/var/lib/fspec/trash/<generation>/` | App data the system scope removed |
| `~/.local/state/fspec/trash/<generation>/` | App data the user scope removed |
| `.../trash/<generation>/manifest.json` | Each item's package and original path |

`fspec gc --older-than` empties old trash.

## Caches

| Path | Holds |
| --- | --- |
| `~/.cache/fspec/loads/` | `gh:` load files, by owner, repo and commit |
| `~/.cache/fspec/spec/<commit>/` | On a remote host: the spec a remote apply shipped |
| `~/.cache/fspec/build/<name>-<platform>/` | Where a package builds: its checkout, build directory and `FSPEC_CACHE`, kept between revs |
| `~/.cache/fspec/jobs/<name>-<rev>-<id>/` | On a builder: one build job, deleted when it finishes |
| `~/.cache/fspec/dev/farms/<hash>/bin` | A project environment's commands |
| `~/.cache/fspec/dev/run/<project>/logs` | Logs of a project's `exec` services |
| `~/.local/share/fspec/store/` | Packages a project environment unpacked for you |
| `~/.local/state/fspec/sync/<item>/` | Sync bases and previous versions |

## Services

| Init | System units | User units |
| --- | --- | --- |
| systemd | `/etc/systemd/system/fspec-<name>.service` (and `.timer`) | `~/.config/systemd/user/fspec-<name>.service` |
| launchd | `/Library/LaunchDaemons/fspec.<name>.plist` | `~/Library/LaunchAgents/fspec.<name>.plist` |
| runit | `/etc/sv/fspec-<name>/` | `~/.local/service/fspec-<name>/` |
| OpenRC | `/etc/init.d/fspec-<name>` | none |
| dinit | `/etc/dinit.d/fspec-<name>` | `~/.config/dinit.d/fspec-<name>` |
| s6 | `/etc/s6/sv/fspec-<name>/` | none |
| cron timers | `/etc/cron.d/fspec-<name>` | a marked block in your crontab |

## Keys

| Path | Meaning |
| --- | --- |
| `~/.config/fspec/age.key` | An age identity, tried after the ssh keys when decrypting secrets |
| `~/.config/direnv/lib/fspec.sh` | Where `fspec dev direnv` output goes |

## Logs

| Path | Meaning |
| --- | --- |
| `<root>/var/log/fspec-commands.log` | Commands a `--root` run would have executed |
| `/var/log/fspec-install.log` | The installer's output |
