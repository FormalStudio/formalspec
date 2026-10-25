+++
title = "Removal and rollback"
description = "What happens when you delete a line from the spec, where app data goes, how to roll back a generation, and how to clean up old ones."
+++

Deleting a line from the spec removes that thing from the machine on the next `fspec apply`:
the package, the services and files declared on it, and its app data. The data moves to a trash
directory instead of being deleted, so `fspec rollback` or adding the line back brings all of it
back. `fspec gc` deletes old generations and their trash once you no longer need them.

This page uses terms from [How FormalSpec works](../concepts/how-it-works.md): a scope (system or
user), state, and a generation.

## Remove a package

1. Delete the `pkg(...)` line from the spec.
2. Preview the removal:

   ```console
   $ fspec plan
   host laptop
     - pkg:lazygit  aqua:lazygit (data to trash: ~/.config/lazygit, ~/.local/state/lazygit)
     0 to add, 0 to change, 1 to remove
   ```

   The removal line lists every path of app data that will move to the trash.

3. Apply:

   ```sh
   fspec apply
   ```

### What removal does, in order

When a `pkg` is gone from the spec, apply:

1. Stops and removes the services and timers declared on it with
   `pkg(services = [...], timers = [...])`.
2. Uninstalls it. A store package drops out of the new generation; a distro package, Flatpak or
   Homebrew formula is removed by its package manager.
3. Deletes the files declared on it with `pkg(files = [...])`, if each still has the content
   fspec wrote. A file that you or the app edited is kept, and apply prints
   `fspec: keeping <path>: it changed since fspec wrote it`. A `patch` file has the keys the spec
   owned taken back out, and the rest of the file stays.
4. Moves its app data to the trash of this generation.

Only `files = [...]`, `services = [...]` and `timers = [...]` tie a resource to a package. A file
that a module returns next to a `pkg(...)` is a resource of its own, and stays until you delete
its own line.

```starlark
pkg("syncthing",
    services = [service("syncthing", exec = "/opt/fspec/current/bin/syncthing serve", user = True,
                        restart = "on_failure")],
    files = [file("~/.config/syncthing/config.xml", mode = "seed", src = "syncthing/config.xml")],
    data = ["~/.local/state/syncthing"])
```

Removing that `pkg` stops the service, removes the binary, deletes `config.xml` if it is
unchanged, and trashes `~/.local/state/syncthing`.

The store directory under `/opt/fspec/store` stays until [`fspec gc`](#clean-up-with-fspec-gc)
deletes it. That is what makes a rollback instant.

## App data and the trash

App data is what an app writes on its own: settings, caches, databases, logins. fspec finds a
package's app data in three ways.

| How | Example |
| --- | --- |
| Declared on the package | `pkg("steam", source = flatpak("com.valvesoftware.Steam"), data = ["~/Games"])` |
| Known by the source | A Flatpak's `~/.var/app/<id>`. For common tools (neovim, helix, lazygit, gh, kitty, ghostty and others), their directories under the four XDG directories |
| Attributed after it ran | A new entry under `~/.config`, `~/.local/share`, `~/.local/state` or `~/.cache` named like the package |

Attribution runs on every user-scope apply, even one with nothing to change. The first apply
records what is in the four XDG directories. Each later apply looks at the entries that are new
since then and claims those named like an installed package: its name, a Flatpak id or the last
part of it, an npm scope or the name after it, ignoring case. So an app's data is attributed on
the first apply after the app first ran. A claim is dropped once the package or the path is
gone.

Anything that none of the three ways claims shows up in [`fspec orphans`](#claim-or-delete-orphans).

### Where trashed data goes

| Path lives in | Trash |
| --- | --- |
| Your home | `~/.local/state/fspec/trash/<generation>/` |
| Anywhere else | `/var/lib/fspec/trash/<generation>/` |

Each trash directory has a `manifest.json` that records every item's package and original path.
When the system scope trashes a path in your home (Flatpak data, since Flatpaks are system
packages), the directory is named `system-<generation>`, so it never collides with the user
scope's own numbers.

The move happens after every package manager has removed the package, under the apply's
journal, so a failing apply puts the data back where it was.

### Getting data back

A package that returns, through `fspec rollback` or because you added its line again, gets back
the data that the newest generation trashed for it. If something already exists at the original
path again (the app ran and created a fresh one), the trashed copy stays in the trash and you can
copy from it by hand.

### Keep data on removal

`keep_data = True` leaves a package's app data in place when you remove it:

```starlark
pkg("obs-studio", source = flatpak("com.obsproject.Studio"), keep_data = True)
```

Use it when you remove a package to reinstall it from another source, or when its data
directory also holds files you manage with `file(...)`. The source-known directories are whole
directories such as `~/.config/nvim`, so removing `neovim` without `keep_data` trashes your
`init.lua` with it, and the next apply writes it back as a new file.

## Claim or delete orphans

`fspec orphans` lists the entries directly under `~/.config`, `~/.local/share`, `~/.local/state`
and `~/.cache` that no package claims and that hold no managed file:

```console
$ fspec orphans
~/.config/oldapp
~/.local/share/oldapp
```

These are leftovers of apps you installed outside fspec, or of packages that no rule could
attribute.

To record one as an installed package's data, so that removing the package trashes it:

```console
$ fspec orphans --claim ~/.local/share/oldapp oldapp
~/.local/share/oldapp now belongs to oldapp
```

The path must be a direct child of one of the four directories, and the package must be
installed, or the command fails with `pkg:oldapp is not installed`.

To move one to the user trash:

```console
$ fspec orphans --delete ~/.config/oldapp
moved ~/.config/oldapp to the trash
```

No package brings an orphan back. It stays in `~/.local/state/fspec/trash/` until `fspec gc`
deletes it, so you can move it back by hand until then. A path that a package claims or a
managed file sits in is refused:

```
Error: ~/.config/git is claimed by a package or covered by a managed file, so it is not an orphan
```

## Roll back

`fspec rollback` re-applies an earlier generation as a new generation:

```sh
fspec rollback
```

Without an argument it goes back one generation. To go to a specific one, give its number:

```sh
fspec rollback 12
```

Rollback runs the same transaction as an apply, with the resources that generation had. Store
packages come back from their store directories, which are still on disk, so nothing is
downloaded. Distro packages and Flatpaks the generation had are installed again by their package
manager. App data comes back from the trash, as described in
[Getting data back](#getting-data-back).

### Rollback and the lock

Rollback does not read `fspec.lock`. It rebuilds a lock from the versions the generation
recorded, and leaves your `fspec.lock` and your spec untouched. So right after a rollback,
`fspec plan` shows the difference between the rolled-back machine and the spec again: a removed
package as a removal, a newer locked version as drift. To make the rollback stick, revert the
spec and the lock in git too:

```sh
git revert HEAD
fspec apply
```

Or apply without reverting to go forward again.

### Generations are per scope

Each scope numbers its generations on its own, so the system scope can be at generation 30
while the user scope is at 12. `fspec rollback` without a number steps each scope back by one.
`fspec rollback <n>` rolls back each scope that has a generation `n`, and leaves a scope whose
current generation is lower than `n` alone. Use `--scope` to act on one:

```sh
fspec rollback 28 --scope system
```

Generation 0 is the state before the first apply. `fspec rollback 0` removes everything fspec
installed in that scope.

## Generations

Every apply that changes something creates a generation. A generation is:

| What | Where |
| --- | --- |
| A snapshot of the scope's state | `/var/lib/fspec/generations/<n>.json`, `~/.local/state/fspec/generations/<n>.json` |
| The links to store packages (system scope) | `/opt/fspec/generations/<n>/` |
| Its trash, when it removed app data | `trash/<n>/` next to the state |

`/opt/fspec/current` points at the active generation's links, and switching to a new one is one
atomic rename. To see which generations exist, list the snapshots:

```sh
ls /var/lib/fspec/generations ~/.local/state/fspec/generations
```

[Paths](../reference/paths.md) lists every file fspec writes.

## Clean up with fspec gc

`fspec gc --older-than <age>` deletes what old generations leave behind. Units are `s`, `m`,
`h`, `d` and `w`.

```console
$ fspec gc --older-than 14d
removed /opt/fspec/store/3f9a1c2e-fd-10.4.2 (2.1 MiB)
removed /var/lib/fspec/generations/27.json (12.4 KiB)
System: freed 2.1 MiB
removed ~/.local/state/fspec/trash/9 (88.0 MiB)
User: freed 88.0 MiB
```

It works per scope. A generation is kept when it is the current one or its snapshot is newer
than the cutoff. For every other generation last changed at or before the cutoff, gc deletes:

- its snapshot `generations/<n>.json` and its trash directory;
- in the system scope, store paths that no kept generation and nothing under `current` links to,
  with their `.json` sidecars and old `/opt/fspec/generations/<n>` link directories;
- in the user scope, [project environments](dev-environments.md) not used within the cutoff or
  whose project is gone, and the user-store packages none of the remaining ones uses.

A deleted generation cannot be rolled back to, and its trashed data is gone for good.
`--older-than 0s` deletes everything the current generation does not need.

### Run gc on a timer

Declare two [timers](services.md): one as root for the system scope, one as you for the user
scope.

```starlark
timer("fspec-gc", exec = "/usr/local/bin/fspec gc --older-than 14d --scope system",
      schedule = "30 0 * * *")
timer("fspec-gc-user", exec = "/usr/local/bin/fspec gc --older-than 14d --scope user",
      schedule = "@daily", user = True)
```

A system timer runs as root, whose home is not yours, so the user-scope cleanup needs its own
timer.

## Apply is a transaction

An apply either completes or leaves the machine as it was. Each scope runs these steps:

1. **Recover.** If the last apply in this scope left a journal behind, it is replayed backwards
   first.
2. **Plan.** fspec diffs the spec against the state. A resource no backend can apply on this
   host stops the apply with `cannot apply:` before anything changes.
3. **Prepare.** Every artifact is downloaded into the store and checked against its hash, and
   every file is rendered into a staging directory. Nothing on the live system has changed yet,
   so a network failure stops the apply cleanly.
4. **Journal.** fspec opens `journal.json` in the scope's state directory. Before each change it
   writes how to undo it, and syncs that to disk.
5. **Commit.** Removals run first, in reverse order: system layer, macOS settings, services,
   store packages, other packages, distro repositories and base packages, files, users. Then
   app data moves to the trash. Then additions and changes run in order: users, files, distro
   repositories and base packages, packages, store packages (the generation switch), services,
   macOS settings, system layer. Data of packages that came back is restored last.
6. **Record.** The new state and the generation snapshot are written, and the journal is
   deleted.

If any step of the commit fails, the journal is replayed backwards and the state file is left as
it was:

```
Error: apply failed and was rolled back

Caused by:
    0: files: applying
    ...
```

### Recovery after a crash

A crash, power loss or `kill` during the commit leaves `journal.json` behind. The next
`fspec apply` (or `rollback`) in that scope reverts it before doing anything else, so the
machine returns to the last completed generation, and then the apply runs from there. It never
resumes a half-finished apply.

If an undo step itself fails, the others still run, and the error lists the ones left:

```
apply failed and rollback was incomplete: rollback left 1 step(s) undone:
  ...
```

Fix what the message names (a package manager lock held by another process is the common case),
then run `fspec apply` again.

## State

Each scope keeps one state file:

| Scope | State |
| --- | --- |
| System | `/var/lib/fspec/state.json` |
| User | `~/.local/state/fspec/state.json` |

It records everything fspec owns: each package and where it went, each file with the hash fspec
wrote, each service, the app data attributed to each package, and the last value fspec wrote for
every key of a `patch` file. `plan` is the diff between the evaluated spec and this file, which
is how fspec knows what to remove when a line disappears. Do not edit it by hand.
