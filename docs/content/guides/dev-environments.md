+++
title = "Project environments"
description = "Give a repository its own locked tools, system libraries, environment and services with a dev.star."
+++

A `dev.star` at the root of a repository declares what working on that project needs: command-line
tools, system libraries, environment variables and services such as a database. `fspec update`
locks it in an `fspec.lock` beside it, and `fspec dev` opens a shell with all of it on `PATH`.
Everyone who clones the repository gets the same tool versions. Nothing is installed globally.

If you know Nix, a `dev.star` is a flake's `devShell`, and `use fspec` in an `.envrc` is
`use flake`. [Compared with a Nix devShell](#compared-with-a-nix-devshell) says where the two
differ.

## Create a project environment

1. Create `dev.star` at the repository root. It calls `dev(...)` once:

   ```starlark
   dev(
       packages = [pkg("just"), pkg("jq")],
       libs = [lib("openssl")],
       bins = [bin("cmake")],
       env = {"RUST_LOG": "info"},
       services = [service("postgres", version = "17")],
   )
   ```

2. Lock it. In a directory with a `dev.star` and no `spec.star`, `fspec update` locks the
   project:

   ```console
   $ fspec update
   + jq 1.8.2
   + just 1.59.0
   + image postgres:17 docker.io/library/postgres@sha256:b8faeff7712663c2773a190181e1246e37e364b887611c04658c28b44e2408fe
   + lib openssl
   + bin cmake
   ```

3. Check what the environment holds on this machine:

   ```console
   $ fspec dev plan
   /home/alex/src/app/dev.star (aarch64-darwin)
     pkg just 1.59.0
     pkg jq 1.8.2
     lib openssl: brew openssl@4
     bin cmake: brew cmake
     service postgres: postgres:17 docker.io/library/postgres@sha256:b8faeff7712663c2773a190181e1246e37e364b887611c04658c28b44e2408fe
   ```

4. Open a shell in it:

   ```sh
   fspec dev
   ```

5. Commit `dev.star` and `fspec.lock` together.

`fspec dev` works from any directory inside the project: it uses the nearest `dev.star` at or
above the current directory. `--dir <path>` names another project.

A real example: this is the `dev.star` FormalSpec's own repository uses. It pins Rust through
rustup and asks for a C compiler on Linux:

```starlark
# Rust through rustup's proxies, which act on the name they run as and install the pinned
# toolchain on first use.
RUST = ["cargo", "rustc", "rustdoc", "rustfmt", "cargo-fmt", "cargo-clippy", "clippy-driver"]

dev(
    packages = [pkg("rustup", links = {name: "rustup" for name in RUST})],
    bins = [bin("gcc")] if platform.is_linux else [],
    env = {"RUSTUP_TOOLCHAIN": "1.99.1"},
)
```

## Use the environment

| Command | Does |
| --- | --- |
| `fspec dev` | Opens `$SHELL` in the environment and starts the project's services |
| `fspec dev -- cargo build` | Runs one command in the environment |
| `fspec dev plan` | Shows what the environment holds and what the lock could not resolve. `fspec plan` does the same in a directory with no `spec.star` |
| `fspec dev up` | Starts the services and keeps them running |
| `fspec dev down` | Stops the services |
| `fspec dev export` | Prints the environment as bash, for direnv |
| `fspec dev direnv` | Prints the direnv library that defines `use fspec` |

The shell gets your environment plus:

- the project's commands first on `PATH`,
- the variables in `env`,
- `FSPEC_DEV`, the project directory,
- `FSPEC_PKG_<NAME>` for each package, the path of its tree (`pkg("yt-dlp")` gives
  `FSPEC_PKG_YT_DLP`), so `env` and `shell_hook` can point into it,
- whatever `shell_hook` exports.

`shell_hook` is `sh` code that runs after the environment is set. The variables it exports are
the ones the shell gets:

```starlark
dev(
    packages = [pkg("bun")],
    shell_hook = "export DATABASE_URL=postgres://localhost:5432/app",
)
```

### With direnv

direnv loads the environment when you `cd` into the project and unloads it when you leave.

1. Install the `use fspec` function:

   ```sh
   mkdir -p ~/.config/direnv/lib
   fspec dev direnv > ~/.config/direnv/lib/fspec.sh
   ```

2. In the project, create `.envrc`:

   ```sh
   use fspec
   ```

3. Allow it:

   ```sh
   direnv allow
   ```

`use fspec` runs `fspec dev export` and reloads when `dev.star` or `fspec.lock` changes. A direnv
shell does not start services, because direnv has no hook for leaving a directory. Run
`fspec dev up` for them. A project with a `container` does not work with direnv; open it with
`fspec dev`.

## Packages

`packages` are ordinary `pkg(...)` declarations with the same sources a host uses; see
[Packages](packages.md). `version` pins an exact upstream version, and `links` adds a command name
that runs another of the package's executables.

Three sources matter most for projects:

| Declaration | Gives |
| --- | --- |
| `pkg("python", version = "3.13")` | CPython from python-build-standalone |
| `pypi("yt-dlp")` | A PyPI distribution and its dependencies, in a virtualenv over the project's python |
| `pkg("model", source = url("https://..."))` | One file at a fixed URL, pinned by hash. An executable becomes a command; any other file is data |

`pypi(...)` needs a `pkg("python", version = ...)` in the same `dev.star`. `fspec update` has
uv compile the package and its dependencies for every platform at once and keeps the hashed
requirements in the lock. uv is added as a package when `dev.star` does not name it. The
virtualenv is built for the machine, so `pypi` packages do not work with `container`.

Packages unpack into the same side-by-side store directories a host uses. A package the machine
already has under `/opt/fspec/store` is used from there. Anything else unpacks into your own store
at `~/.local/share/fspec/store`. The commands are linked from
`~/.cache/fspec/dev/farms/<hash>/bin`. No root is needed.

## System libraries and tools

Some projects need libraries and tools from the operating system: a C compiler, `openssl`
headers, `gtk4`. `lib` and `bin` name them by what they provide, not by a package name, and
`fspec update` asks each package manager which package provides it:

```starlark
dev(
    libs = [lib("gtk4"), lib("openssl"), lib("gstreamer-1.0/libgstlibav.so")],
    bins = [bin("cmake"), bin("pkg-config")],
)
```

- `lib("gtk4")` is the pkg-config module `gtk4` (the file `gtk4.pc`).
- A name ending in `.so` is a shared object under the library directory, for a plugin that ships
  no pkg-config module.
- `bin("cmake")` is an executable.

| Manager | `lib("gtk4")` | `bin("cmake")` |
| --- | --- | --- |
| dnf | `pkgconfig(gtk4)` | `/usr/bin/cmake` |
| zypper | `pkgconfig(gtk4)` | `zypper se --file-list` |
| moss | `pkgconfig(gtk4)` | `binary(cmake)` |
| apt | `apt-file search` for `gtk4.pc` | `apt-file search` for `bin/cmake` |
| pacman | `pacman -F gtk4.pc` | `pacman -F cmake` |
| xbps | `xlocate` | `xlocate` |
| brew | the formula whose bottle ships `lib/pkgconfig/gtk4.pc` | the formula that lists `cmake` in its executables |

The apt, pacman, xbps and zypper lookups run in a container of that distro, so `fspec update`
needs docker or podman for them.

When a lookup finds several packages, the one with the same name (for a `bin`) or the only
`-dev`/`-devel` one (for a `lib`) wins. Otherwise the lookup is a miss. A miss is recorded in the
lock with its reason, and `fspec update` and `fspec dev plan` print it. Name the package by hand
for that manager:

```starlark
lib("x11", apt = "libx11-dev", pacman = "libx11")
```

`lib` and `bin` take `apt`, `dnf`, `zypper`, `pacman`, `xbps`, `moss` and `brew`.

A lookup that cannot run at all, such as no network or no docker, is not a miss. The lock keeps
what it had for that manager and `fspec update` fails, so a flaky run never records misses that
are not true.

### What activation checks

`fspec dev` installs nothing system-wide. It checks that each `.pc` file or executable is present
and fails with the install command for what is missing:

```console
$ fspec dev
Error: dev.star needs system packages:
  not installed, install with: brew install cmake
```

On macOS, each brew formula's `bin`, `lib`, `lib/pkgconfig` and `include` go on `PATH`,
`LIBRARY_PATH`, `PKG_CONFIG_PATH` and `CPATH`, since Homebrew's prefixes are not on the default
search paths.

### When the machine cannot provide them

A machine whose package manager has no package for some `lib` or `bin` (Void has no
`libsystemd`), or that has no package manager fspec knows (NixOS), cannot provide them natively.
`fallback_container` runs the shell in an image on those machines only:

```starlark
dev(
    libs = [lib("libsystemd")],
    fallback_container = "debian:13",
)
```

`fspec dev` then prints which dependencies sent it there:

```console
fspec: xbps has no package for lib libsystemd; running in debian:13 (fallback_container)
```

Without `fallback_container`, activation fails and names the misses. Nothing is skipped silently.

## Containers

`container` runs every shell of the project in a pinned image, on every machine:

```starlark
dev(
    packages = [pkg("just")],
    libs = [lib("gtk4")],
    container = "debian:13",
)
```

- `fspec update` pins the image by digest.
- The first `fspec dev` builds one derived image on top of it that installs the locked `lib` and
  `bin` packages (and `ca-certificates`) from that distro's repositories. It is tagged
  `fspec-dev:<hash>` by its Containerfile, so it is built again only when that changes.
- The shell runs with the host network and your uid. The project and a per-project home are
  mounted read-write, and the store and the command links read-only, at the same paths they have
  outside.
- On macOS the container runs the `<arch>-linux` evaluation of `dev.star`.

The image must be a debian, ubuntu, fedora, archlinux, opensuse or void image, so fspec knows
which package manager installs the `lib` and `bin` packages. It needs docker or podman.

## Services

`services` are processes or containers that run while you work on the project:

```starlark
dev(
    packages = [pkg("bun")],
    services = [
        service("postgres", version = "17", env = {"POSTGRES_PASSWORD": "dev"}),
        service("redis", image = "docker.io/library/redis:7", ports = [6379]),
        service("worker", exec = "bun run worker.ts"),
    ],
)
```

| Form | Runs |
| --- | --- |
| no `exec` | The image `<name>:<version>` (`<name>:latest` without a version), or `image`, pinned by digest in the lock. Each exposed port is published on `127.0.0.1`, or the ports you list in `ports`. Each volume the image declares gets a named volume, so data survives restarts |
| `exec` | A command started from the project's environment in the project directory, in its own process group. Its log is under `~/.cache/fspec/dev/run/<project>/logs` |

Services start when the first `fspec dev` shell of the project opens and stop when the last one
exits. `fspec dev up` starts them and keeps them running until `fspec dev down`. Image services
need docker or podman. Service names take letters, digits, `-` and `_`.

## Platforms

`dev.star` is evaluated once per platform, with a `platform` global (`name`, `os`, `arch`,
`is_linux`, `is_darwin`), so a file can branch the way a flake does on `stdenv.isLinux`. `fspec
update` locks every platform at once.

`platforms` limits the project to some of the four (`x86_64-linux`, `aarch64-linux`,
`x86_64-darwin`, `aarch64-darwin`):

```starlark
dev(
    packages = [pkg("just")],
    platforms = ["x86_64-linux", "aarch64-darwin"],
)
```

On any other platform, `fspec dev plan` says the project does not support this machine, and
`fspec dev` refuses to start. `platforms` itself cannot depend on `platform`.

## Compared with a Nix devShell

A `dev.star` is weaker than a Nix devShell, and it helps to know where.

- **Packages** are pinned as exactly as Nix pins them: one version and one hash per platform,
  the same bytes on every machine.
- **`lib` and `bin`** pin only a package name. The version is whatever the distro, or Homebrew,
  ships that day, so two developers on Fedora and Arch build against different gtk4 releases. A
  Nix devShell pins every library to a store path built from a hashed recipe.
- **`container`** narrows that to one distro and one base image digest, but the derived layer
  still installs the repository's current versions when it is built.

Making system libraries exact would mean building them from source (which is what Nix does) or
snapshot mirrors for every distro. FormalSpec does neither. What it gives in return is speed: no
evaluation of a package set, nothing built from source, and libraries from the package manager
you already have.

## How it works

- `fspec update` in a project writes every package's artifacts for every platform, the digest of
  the container and of each service image, and the package that provides each `lib` and `bin` on
  each package manager. Activation reads the lock and never resolves anything.
- The command links live in a link farm, `~/.cache/fspec/dev/farms/<hash>`, one per project and
  set of packages.
- `fspec gc` drops link farms whose project is gone or that were not used within the cutoff, then
  the user-store packages no remaining farm links into.

## Troubleshooting

| Message | Fix |
| --- | --- |
| `no dev.star at or above <dir>` | Run the command inside the project, or pass `--dir` |
| `<dir>/dev.star has no fspec.lock beside it, run fspec update there` | Lock the project first |
| `pkg <name> is not in fspec.lock, run fspec update` | You added a package after the last update |
| `dev.star needs system packages: ...` | Install what the message lists with the command it prints |
| `<lib or bin> was not resolved for <manager>, run fspec update` | The lock has no answer for this machine's package manager |
| `this system has no package manager fspec checks lib(...) and bin(...) against; set fallback_container = "debian:13" in dev.star to get them from an image` | Add `fallback_container`, as the message says |
| `<dir>/dev.star does not support <platform> (platforms = [...])` | The project's `platforms` leaves out this machine |
| ``<dir>/dev.star runs in the container <image>; open it with `fspec dev` `` | `fspec dev export` and direnv do not work with `container` |
| `container = ... needs docker or podman` | Install docker or podman |
| `service <name> runs an image, which needs docker or podman` | Install docker or podman, or give the service an `exec` |
| `pypi(...) packages install over a pkg("python", version = ...), and dev.star declares none` | Add `pkg("python", version = "3.13")` |
| `pypi(...) packages are installed for this machine and cannot run in a container yet` | Drop `container`, or install the tool another way |
| `shell_hook in dev.star failed (...)` | The hook exited with an error. Run it in `sh` to see why |
