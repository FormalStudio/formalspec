+++
title = "How FormalSpec works"
description = "The spec, the lock, state, scopes, generations and the store, and what plan and apply do with them."
+++

FormalSpec compares two things: what your spec says a host should have, and what FormalSpec
installed on it last time. `plan` prints the difference. `apply` makes the host match the spec and
records the result. This page explains each piece and how they fit together.

## The pieces

| Piece | Where | Written by |
| --- | --- | --- |
| The spec | `spec.star` and the files it loads, in a git repository | You |
| The lock | `fspec.lock`, next to `spec.star` | `fspec update` |
| State | `/var/lib/fspec/state.json` and `~/.local/state/fspec/state.json` | `fspec apply` |
| The store | `/opt/fspec/store`, with generations and `current` | `fspec apply` |

You commit the spec and the lock. State and the store live on each machine and are never
committed.

## The spec

The spec is a Starlark program. Starlark reads like Python, and it is deterministic: a spec cannot
read files outside its repository, open a network connection or look at the clock. Evaluating
the same commit always produces the same list of resources.

```starlark
load("//modules/cli.star", "cli")

hosts = [
    host(name = "laptop", platform = "aarch64-darwin", modules = [cli]),
    host(name = "desktop", platform = "x86_64-linux", system = fedora(), modules = [cli]),
]
```

Evaluation turns each host into a list of *resources*: packages, files, services, users and the
rest of the builtins in the [reference](../reference/builtins.md). Facts about the host, such as
its platform and distro, come from the `host(...)` declaration, never from probing the machine.
`apply` checks that the machine matches the declaration and stops if it does not:

```
host desktop is declared as x86_64-linux but this machine is aarch64-darwin
```

[Hosts and modules](hosts-and-modules.md) covers how to organise a spec.

## The lock

`fspec.lock` holds every version FormalSpec installs: for each package, the exact version, and
for each platform a host declares, the download URL and its hash. `fspec update` is the only
command that talks to upstream sources and the only one that writes the lock. `plan` and `apply`
read it and nothing else, so two hosts at the same commit install the same bytes.

```console
$ fspec update
+ fd 10.5.0
+ ripgrep 15.2.0
```

A package that is in the spec but not in the lock fails the plan with `run fspec update`.
[The lock file](../reference/lock-file.md) documents every entry.

## State

State records what FormalSpec owns on a machine: each package and where it went, each file with
the hash of what was written, each service, and the app data it attributed to a package. A plan
is the difference between the evaluated spec and this record.

Because FormalSpec only acts on what it recorded, it never touches anything else. A file you
created by hand, a package you installed with `brew` yourself, your documents: none of these are
in state, so none of them are changed or removed.

## Scopes

Every resource belongs to one of two scopes, and each scope is applied as its own transaction
with its own state:

| Scope | Runs as | Holds |
| --- | --- | --- |
| system | root | Store packages, distro packages, Flatpaks, files outside `~`, system services, users, the system layer |
| user | you | Files under `~`, user services, Homebrew, macOS `defaults` for your account |

When you run `fspec apply` as a normal user, it runs the system scope through
`sudo fspec apply --scope system`, then the user scope as you. That is why the output of an apply
has two blocks. Pass `--scope system` or `--scope user` to apply one.

## Plan

```console
$ fspec plan
host laptop
  + pkg:ripgrep  aqua:ripgrep
  ~ file:~/.config/fish/config.fish  managed [content]
  ! file:~/.config/git/ignore drifted: content changed
  - pkg:fd  github:sharkdp/fd
  1 to add, 2 to change, 1 to remove
```

Each line is a resource, named `kind:id`, with a sigil:

| Sigil | Meaning |
| --- | --- |
| `+` | In the spec, not on the machine yet. Apply adds it |
| `-` | On the machine, gone from the spec. Apply removes it |
| `~` | In both, but the spec changed. The brackets list the fields |
| `!` | Unchanged in the spec, but the machine no longer matches: drift. Apply repairs it |
| `>` | A removed file whose path another resource now writes. Apply leaves it on disk |
| `?` | No backend on this host can handle the resource. Apply refuses |

Drift is how FormalSpec notices changes made outside it: a file someone edited, a service that
was disabled, a package that is missing, a lock bump that has not reached the host yet.

`plan` never changes anything. `fspec plan --all` plans every host in the spec, asking remote
hosts over ssh.

## Apply

An apply is a transaction. It runs in this order:

1. **Prepare.** Download every artifact into the store and check its hash. Render every file
   into a staging directory. Nothing on the live system has changed yet, so a network failure
   stops the apply cleanly.
2. **Journal.** Write down every change about to be made, with how to undo it.
3. **Snapshot.** On a classic distro with a btrfs root, take a read-only snapshot before a
   change to the system layer.
4. **Commit.** Make the changes in a fixed order: removals first, then users, files, store
   packages, distro and Flatpak packages, services, macOS defaults, and the system layer last.
5. **Record.** Write the new state and a new generation, only after every step succeeded.

If a step fails, the journal is replayed backwards and the host returns to where it started. If
the machine crashes in the middle, the next run finds the journal and reverts it.

## The store and generations

Packages that FormalSpec downloads itself (from aqua, GitHub, npm, cargo, a URL or a source
build) never go into `/usr`. They unpack into their own directory:

```
/opt/fspec/store/<hash>-<name>-<version>/
```

A *generation* is a directory of symlinks into the store, and `/opt/fspec/current` points at the
active one. You put `/opt/fspec/current/bin` on your `PATH`. Switching generations is a single
rename of the `current` link, so it is atomic. Two versions of a tool can sit in the store side
by side, and a project can pick one in its [`dev.star`](../guides/dev-environments.md).

`/opt/fspec/current/pkgs/<name>` links each package's whole tree, for config that needs an
absolute path to a library or data file inside a package.

Distro packages, Flatpaks and Homebrew packages live where their package manager puts them.
FormalSpec still records them in state, so removal and rollback cover them too.

## Rollback

`fspec rollback` re-applies the previous generation as a new one. Old store directories are still
on disk, so packages come back without a download. Removed files, services and app data come
back too. Rollback does not touch `fspec.lock`; until you revert the spec as well, `plan` reports
the difference. [Removal and rollback](../guides/removal-and-rollback.md) has the details, and
`fspec gc` removes old generations.

## What FormalSpec does not do

- It does not build software that a distro or upstream already ships as a binary. Source builds
  are for packages nothing else provides, and CI builds them once.
- It does not evaluate a whole package collection. A spec evaluates only the hosts and modules
  it names.
- It does not replace your operating system. The distro's package manager owns `/usr`.
