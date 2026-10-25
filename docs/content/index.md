+++
title = "FormalSpec"
description = "Declare your machines in Starlark and keep them identical, on any Linux distro and on macOS."
+++

FormalSpec describes your machines in code. You write a spec in Starlark, a small dialect of
Python. `fspec` pins every version in one lockfile, and every host that applies the same commit
ends up with the same software, files and services.

```starlark
hosts = [
    host(
        name = "laptop",
        platform = "aarch64-darwin",
        modules = [
            pkg("ripgrep"),
            pkg("fd", source = github("sharkdp/fd")),
            file("~/.config/git/ignore", content = ".DS_Store\n"),
        ],
    ),
]
```

```console
$ fspec update
+ fd 10.5.0
+ ripgrep 15.2.0
$ fspec apply
host laptop
  + pkg:fd  github:sharkdp/fd
  + pkg:ripgrep  aqua:ripgrep
  2 to add, 0 to change, 0 to remove
```

## How it differs

FormalSpec runs on top of the operating system you already have and drives its package manager
(dnf, pacman, apt, zypper, xbps, Homebrew) instead of replacing it. It has no central package
collection. Packages come straight from where upstream publishes them: GitHub releases, npm,
crates.io, Flathub, Homebrew and your distro's repositories. `fspec update` finds new releases and
writes exact versions and hashes into `fspec.lock`, so you never type a hash and never wait for
someone else to bump a package.

Removing a line from the spec removes the thing from the machine on the next apply: the package,
its services, its files and its app data. `fspec rollback` brings all of it back.

If you know Nix, FormalSpec keeps the parts you would miss (one lock, generations, rollback, clean
removal) and drops nixpkgs and the build graph. [Coming from Nix](from-nix.md) maps the
constructs one by one.

## What it manages

| Area | What you declare |
| --- | --- |
| Packages | Prebuilt releases pinned by hash, distro packages, Flatpaks, Homebrew formulae and casks, npm and cargo tools |
| Files | Configs written, linked, seeded once, or merged key by key into a file the app also writes |
| Secrets | age-encrypted files, decrypted at apply |
| Services | One description, rendered for systemd, launchd, runit, OpenRC, dinit or s6 |
| System | A bootc image (Fedora Atomic, Bazzite, Aurora) or a classic distro's base packages and repos |
| macOS | `defaults` keys, `pmset` and firewall settings, app bundles in `/Applications` |
| Projects | Per-repo tools and libraries in a `dev.star`, the equivalent of a devShell |
| Hosts | Several machines from one repo, applied locally or over ssh |

## Where to start

1. [Install fspec](install.md) on the machine you want to manage.
2. Follow [Your first spec](first-spec.md) to declare a host, apply it and roll it back. It
   takes about ten minutes.
3. Read [How FormalSpec works](concepts/how-it-works.md) for the model behind `plan` and `apply`.

After that, the guides cover one area each, and the reference lists every
[command](reference/commands.md) and [builtin](reference/builtins.md).

## Status

FormalSpec is early. Apply, drift repair, removal, rollback and `gc` have run on real machines:
macOS, and Fedora 44, Arch, Debian 13 and Void in containers, locally and over ssh. bootc images
and `fspec switch` have only run against test roots so far. Expect breaking changes between
versions.
