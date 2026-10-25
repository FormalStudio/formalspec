+++
title = "Install fspec"
description = "Build the fspec binary from source with cargo or Nix, and check that it runs."
+++

FormalSpec is a single binary, `fspec`. It has no daemon and no background service. You install it
on every machine you want to manage, and on the machine you edit the spec from if that is another
one.

There are no prebuilt releases yet, so you build it from the repository. To put FormalSpec on a
blank machine instead, boot the [installer](guides/installer.md), which carries its own `fspec`.

## Supported platforms

| Platform | Notes |
| --- | --- |
| `x86_64-linux`, `aarch64-linux` | Fedora, Fedora Atomic and Universal Blue images, Arch, Debian, Ubuntu, Void, openSUSE Tumbleweed, AerynOS |
| `aarch64-darwin` | macOS on Apple silicon |
| `x86_64-darwin` | Builds, but no host has run it |

## Install with cargo

Prerequisites:

- [rustup](https://rustup.rs) with a recent stable toolchain. FormalSpec uses the 2024 edition.
- git, and a C compiler (`gcc` or `clang` on Linux, the Xcode Command Line Tools on macOS).

1. Install the binary from the main branch:

   ```sh
   cargo install --git https://github.com/FormalStudio/formalspec --locked fspec
   ```

   `--locked` builds with the dependency versions the repository was tested with.

2. Check that it runs:

   ```sh
   fspec --version
   ```

cargo puts the binary in `~/.cargo/bin`. That is enough for `fspec apply` on this machine: when
it needs root for the system scope, it runs itself through `sudo` by its full path.

## Install with Nix

The repository is a flake with a `fspec` package for Linux and macOS:

```sh
nix profile install github:FormalStudio/formalspec
```

A Nix build links against the Nix store, so it does not run in a fresh root that
[`fspec switch`](guides/switching-distros.md) bootstraps. Use a static build for that.

## Make it reachable over ssh

To [apply a host over ssh](guides/remote-hosts.md), `fspec` must be on the `PATH` of a
non-interactive ssh shell on that host. `~/.cargo/bin` often is not. Check from the machine you
drive the apply from:

```sh
ssh my-host fspec --version
```

If that fails with `command not found`, link the binary somewhere on the default path:

```sh
sudo ln -s ~/.cargo/bin/fspec /usr/local/bin/fspec
```

Every host must run an fspec at least as new as the one that drives the apply. An older one
refuses with `fspec on <host> is older than this one`.

## Optional tools

`fspec` calls other programs for some features. You only need the ones for features you use.

| Tool | Needed for |
| --- | --- |
| `gh` or `GITHUB_TOKEN` | More than 60 GitHub API requests an hour during `fspec update`, and private `gh:` loads |
| `npm` | npm packages with runtime dependencies |
| `podman` or `docker` | `git(...)` builds with `image =`, `lib()` lookups for apt, pacman and xbps, `container` in `dev.star` |
| `oras` | Pulling CI-built packages from an OCI registry |
| `secret-tool` | Keyring items in [`fspec sync`](guides/sync.md) |

## Next step

[Your first spec](first-spec.md) declares this machine, applies it and rolls it back.
