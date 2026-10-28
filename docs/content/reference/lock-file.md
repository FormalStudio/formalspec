+++
title = "The lock file"
description = "What fspec.lock holds, how fspec update writes it, and how to read a change in review."
+++

`fspec.lock` is TOML, next to `spec.star` (or next to `dev.star` for a project). `fspec update`
writes it and you commit it. Never edit it by hand: the next update rewrites it, and `plan` and
`apply` trust it to be what `update` wrote.

A lock bump is an ordinary diff. A nightly CI job can run `fspec update` and open a pull request;
merging that pull request is how hosts get updates.

## Packages

One `[[pkg]]` table per package, keyed by name and source. Two hosts can take the same name from
different sources (`brew` on a Mac, `github` on Linux), and each reads its own entry.

```toml
[[pkg]]
name = "ripgrep"
source = "aqua:ripgrep"
version = "15.2.0"

[pkg.aarch64-darwin]
url = "https://github.com/BurntSushi/ripgrep/releases/download/15.2.0/ripgrep-15.2.0-aarch64-apple-darwin.tar.gz"
hash = "sha256-N1Cy6T834MaSZX2ldNcBmhAcAITaBaeQyD/TNbrZc+Q="
bins = ["ripgrep-15.2.0-aarch64-apple-darwin/rg"]
```

| Key | Meaning |
| --- | --- |
| `name` | The package name from the spec |
| `source` | The source, such as `aqua:ripgrep`, `github:sharkdp/fd`, `git:https://...`. A spec whose source no longer matches needs `fspec update` |
| `version` | The upstream version |
| `rev` | For `git(...)`: the exact commit |
| `patches` | For patched packages: a hash over the patch file names and contents, in order |
| `parts` | For `pkg(parts = ...)`: an artifact per path |
| `<platform>` | One artifact per platform a host declares, or `any` for platform-independent ones (npm tarballs, Flatpak commits) |

Each artifact has some of these keys:

| Key | Meaning |
| --- | --- |
| `url` | Where to download it |
| `hash` | `sha256-` or `sha512-` SRI hash, checked before anything is unpacked |
| `oci` | `registry/repo@sha256:...` for a CI-built package |
| `build` | The build key of a CI-built package: eight hex digits over its commit, patches and recipe. A host installs `oci` only when it matches its own |
| `commit` | A Flatpak commit or another exact id |
| `bins` | Executables inside the artifact, relative to its root. `<command>=<path>` names a command differently |
| `requirements` | For `pypi(...)`: the hashed requirement lines uv compiled |

A package with patches that no longer hash to `patches` is refused with
`patches changed, run fspec update`.

### Distro packages

A `dnf`, `pacman`, `apt` or `xbps` package locks its version and the exact package file URL and
hash per platform. FormalSpec caches that file under `/var/cache/fspec/pkgs`, so the pin stays
installable after the distro's repository moves on.

The lookup needs the distro's metadata. Arch and Void metadata can be read from any machine.
Fedora and Debian pins need `fspec update` to run on a machine of that distro and architecture;
elsewhere those packages lock by name only and install the newest version. `moss` and `zypper`
packages always lock by name: AerynOS pins whole-system states, and Tumbleweed pins a snapshot
(below).

Homebrew installs only the current version, so a `brew` entry records the version and drift
reports a mismatch, but an older version cannot be reinstalled.

## System layer

```toml
[system.desktop]
image = "ghcr.io/alex/fspec-desktop@sha256:..."

[zypper.laptop]
snapshot = "20261020"
```

| Table | Meaning |
| --- | --- |
| `[system.<host>]` | The image host's bootc image, pinned by digest. Written by `fspec update --images` |
| `[zypper.<host>]` | The Tumbleweed snapshot the host's repositories point at. Moved by a plain `fspec update` |

openSUSE keeps about a month of snapshots, so a snapshot pin older than that stops resolving and
needs an update.

## Loads

```toml
[loads."gh:FormalStudio/formalmusic@main"]
rev = "0123456789abcdef0123456789abcdef01234567"
files = { "fspec.star" = "sha256-...", "lib.star" = "sha256-..." }
```

One table per `load("gh:...")` source: the commit the ref resolved to and the hash of each file
loaded from it. See [Hosts and modules](../concepts/hosts-and-modules.md#load-a-module-from-another-repository).

## Project environments

A `dev.star` lock also has:

```toml
[[native]]
kind = "bin"
name = "gcc"
apt = "gcc"
dnf = "/usr/bin/gcc"
moss = "binary(gcc)"
pacman = "gcc"

[image]
"debian:13" = "debian@sha256:..."
```

| Table | Meaning |
| --- | --- |
| `[[native]]` | A `lib(...)` or `bin(...)`, with the package that provides it on each package manager, and `miss` with the reason where a lookup found nothing |
| `[image]` | Each container and service image the project names, pinned by digest |

## When fspec update runs

| Command | Updates |
| --- | --- |
| `fspec update` | Every package, every `gh:` load, every Tumbleweed snapshot |
| `fspec update <pkg>...` | Only those packages. Loads stay at their commit |
| `fspec update --cache` | Only the `oci` digests and `build` keys of CI-built packages |
| `fspec update --images` | Only the `[system.<host>]` image digests |

Resolvers read GitHub's API with `GITHUB_TOKEN`, or with what `gh auth token` prints. Without a
token GitHub allows 60 requests an hour; a refusal for that reason is reported as the rate limit,
with when it resets.
