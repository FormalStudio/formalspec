+++
title = "Your first spec"
description = "Declare the machine you are on, pin its packages, apply it, change it and roll it back."
+++

In this tutorial you create a spec repository for the machine you are using, install two
command-line tools and a config file with it, then remove one tool and bring it back. It takes
about ten minutes.

Prerequisites:

- `fspec` on your `PATH` ([Install fspec](install.md)).
- git.
- An account that can run `sudo`. Packages install into `/opt/fspec`, which needs root.

## Create the repository

A spec is a git repository. Its entry point is `spec.star`.

```sh
mkdir ~/spec && cd ~/spec
git init
```

## Find your host name

FormalSpec decides which host in the spec is "this machine" by name: the system host name,
lowercased, up to the first dot. `fspec facts` prints it with the platform and distro:

```console
$ fspec facts
macbook-pro-2: aarch64-darwin/macos
```

This tutorial calls the host `laptop`. Use the name `fspec facts` printed instead, or pass
`--host laptop` to every command.

## Declare the host

Create `spec.star`. Pick the `platform` that matches your machine: `aarch64-darwin` for a Mac
with Apple silicon, `x86_64-linux` or `aarch64-linux` for Linux.

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

On Linux, a host also declares its `system`, the distro it runs. Add the line that matches
yours, for example:

```starlark
        system = fedora(),
```

The spec says three things:

- `pkg("ripgrep")` installs ripgrep from the [aqua registry](https://github.com/aquaproj/aqua-registry),
  the default source for command-line tools.
- `pkg("fd", source = github("sharkdp/fd"))` takes fd from its GitHub releases instead, picking
  the asset for your platform.
- `file(...)` writes `~/.config/git/ignore` with that content.

## Lock the versions

```console
$ fspec update
+ fd 10.5.0
+ ripgrep 15.2.0
```

`fspec update` asks each source for its newest release and writes the version, the download URL
and its hash for your platform into `fspec.lock`:

```toml
[[pkg]]
name = "fd"
source = "github:sharkdp/fd"
version = "10.5.0"

[pkg.aarch64-darwin]
url = "https://github.com/sharkdp/fd/releases/download/v10.5.0/fd-v10.5.0-aarch64-apple-darwin.tar.gz"
hash = "sha256-tn4YNsRo5C5BGYS1blL6er7AjCvSLIZzmOfME0qsXhI="
```

Nothing else resolves versions. `plan` and `apply` read only the lock, so the same commit
installs the same bytes on every machine.

## Preview the change

```console
$ fspec plan
host laptop
  + file:~/.config/git/ignore  managed
  + pkg:fd  github:sharkdp/fd
  + pkg:ripgrep  aqua:ripgrep
  3 to add, 0 to change, 0 to remove
```

`+` adds, `~` changes, `-` removes. `plan` never changes the machine.

## Apply it

```console
$ fspec apply
host laptop
  + pkg:fd  github:sharkdp/fd
  + pkg:ripgrep  aqua:ripgrep
  2 to add, 0 to change, 0 to remove
host laptop
  + file:~/.config/git/ignore  managed
  1 to add, 0 to change, 0 to remove
```

`sudo` asks for your password once. The output has two blocks because an apply runs in two
[scopes](concepts/how-it-works.md#scopes): the system scope as root (packages under `/opt/fspec`),
then the user scope as you (files under `~`).

The tools are now in `/opt/fspec/current/bin`. Put that directory on your `PATH`, for example
in `~/.zshrc`:

```sh
export PATH="/opt/fspec/current/bin:$PATH"
```

Open a new shell and check:

```sh
rg --version
fd --version
```

Commit the spec and the lock together. The lock is what makes the next machine identical.

```sh
git add spec.star fspec.lock
git commit -m "laptop: ripgrep, fd, git ignore"
```

## See drift

Edit the file by hand, then plan again:

```console
$ echo '*.log' >> ~/.config/git/ignore
$ fspec plan
host laptop
  ! file:~/.config/git/ignore drifted: content changed
  0 to add, 1 to change, 0 to remove
```

The next apply puts back what the spec says. To keep your edit instead, copy it into the spec,
or run `fspec adopt ~/.config/git/ignore` for a file that comes from a `src` in the repository.
[Files](guides/files.md) covers the other modes, including one that merges your keys into a file
the app keeps writing.

## Remove a package

Delete the `fd` line from `spec.star` and apply:

```console
$ fspec apply
host laptop
  - pkg:fd  github:sharkdp/fd
  0 to add, 0 to change, 1 to remove
```

`fd` is gone from `PATH`. Removing a line removes everything that came with it: the package, the
services and files declared on it, and its app data, which moves to a trash directory.

## Roll back

```sh
fspec rollback
```

Rollback re-applies the previous generation as a new one. The old store directory is still on
disk, so `fd` comes back without a download. Your spec still lacks the line, so `fspec plan`
now shows the removal again. Put the line back, or apply to drop it for good.

## Next steps

- [How FormalSpec works](concepts/how-it-works.md): the spec, the lock, state, scopes and
  generations.
- [Hosts and modules](concepts/hosts-and-modules.md): split the spec into files and share it
  between machines.
- [Packages](guides/packages.md): every source, and when to pick which.
