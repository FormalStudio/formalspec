+++
title = "Packages"
description = "Pick a source for each package, pin it with fspec update, and control what gets linked onto PATH."
+++

A package is a `pkg(...)` in a host's `modules`. Its `source` says where it comes from: a
prebuilt release on GitHub, your distro's repositories, Flathub, Homebrew, npm, crates.io, a
fixed URL, or a source build. `fspec update` resolves every package to an exact version and hash
in `fspec.lock`, and `fspec apply` installs exactly that.

FormalSpec has no package collection of its own. It takes software from where upstream or your
distro already publishes it, and builds from source only when nobody ships a binary.

## Choose a source

Work down this table and stop at the first row that fits. Binaries that already exist come
first; a source build is the last resort.

| What you install | Source | Example |
| --- | --- | --- |
| A command-line tool with GitHub releases | the aqua registry (default) | `pkg("ripgrep")` |
| A tool the aqua registry lacks, or a release asset you want to pick | `github(repo)` | `pkg("fd", source = github("sharkdp/fd"))` |
| Something that belongs to the OS: drivers, daemons, shells, libraries, desktop sessions | the distro: `dnf`, `pacman`, `apt`, `xbps`, `moss`, `zypper` | `pkg("htop", source = pacman())` |
| A desktop app on Linux | `flatpak(id)` | `pkg("obsidian", source = flatpak("md.obsidian.Obsidian"))` |
| A formula or app on macOS | `brew(name)` | `pkg("wget", source = brew())` |
| A JavaScript tool | `npm(name)` | `pkg("prettier", source = npm("prettier"))` |
| A Rust crate whose GitHub releases carry binaries | `cargo(name)` | `pkg("bat", source = cargo("bat"))` |
| A file at a fixed URL: a font, an AppImage, a tarball with no releases page | `url(url)` | `pkg("bibata", source = url("https://..."))` |
| Software nobody ships a binary of | `git(url, rev)` | see [Building from source](source-builds.md) |
| A nixpkgs build, on a host that already has Nix | `nix(attr)` | `pkg("hello", source = nix("hello"))` |

Some rules of thumb:

- Prefer the distro for anything that links against system libraries or plugs into the OS (a
  PAM module, a kernel module, a display manager). The distro builds it against the libraries
  the host actually has.
- Prefer the aqua registry or `github(...)` for standalone command-line tools. They are pinned
  by hash and the same bytes land on every host.
- Prefer Flatpak for GUI apps on Linux. Flathub ships one build that runs on every distro.
- One host can take a package from one source and another host from a different one. Each host
  reads its own lock entry.

```starlark
def editor(host):
    if host.platform.is_darwin:
        return pkg("zed", source = brew(cask = True))
    return pkg("zed", source = flatpak("dev.zed.Zed"))
```

## Sources

Every source builtin and its keywords are in the [builtins reference](../reference/builtins.md#sources).
This section shows each one in use.

### aqua registry

`pkg(name)` with no `source` looks the name up in the
[aqua registry](https://github.com/aquaproj/aqua-registry), which maps tool names to their
GitHub release assets for each platform.

```starlark
pkg("ripgrep"),
pkg("jq", version = "1.7.1"),
```

`aqua(name)` names the registry entry when it differs from the package name:
`pkg("rg", source = aqua("ripgrep"))`. Only registry entries of type `github_release` and `http`
are supported.

`pkg("python")` is the exception: without a `source`, it installs CPython from
python-build-standalone.

### GitHub releases

`github("owner/repo")` takes the newest stable release and picks the asset that matches the
host's platform. When a release has several candidates, or names its assets in a way the matcher
cannot read, `asset` is a glob over the asset names:

```starlark
pkg("fd", source = github("sharkdp/fd")),
pkg("adw-gtk3", version = "6.5", source = github("lassekongo83/adw-gtk3", asset = "adw-gtk3v*.tar.xz")),
pkg("kopuz", source = github("Kopuz-org/kopuz", asset = "Kopuz_*_aarch64.dmg"), apps = ["Kopuz.app"]),
```

The matcher skips `.dmg` assets unless `asset` names one.

### Distro packages

The distro sources install from the host's own repositories through its package manager. With
no argument, the package's name is the distro package name. A list installs several together.

```starlark
pkg("htop", source = pacman()),
pkg("toolbox", source = dnf()),
pkg("fonts", source = dnf(["google-noto-sans-fonts", "google-noto-serif-fonts"])),
pkg("nordvpn", source = zypper(repo = "nordvpn")),
```

A distro source applies only on a host of that distro. A `dnf()` package on an Arch host shows
in the plan as `? no backend applies pkg:<name> on <host> (arch, systemd)`, and `apply` refuses
while any `?` line remains. On a bootc image host, distro packages go
in `image(packages = ...)` instead. The base packages of a classic distro go in
`system = fedora(packages = [...])`; see [The system layer](system-layer.md).

`zypper(repo = ...)` ties the package to a third-party `repo(...)` on openSUSE Tumbleweed.

On Tumbleweed, `apply` asks zypper's solver about the new base packages before anything
changes (`zypper install --dry-run`), and about the new `pkg` packages together with them when
the base list changes too. Two packages that conflict, such as `rustup` and `rust-src`, stop the
apply with zypper's problem text in seconds, not after the snapshot upgrade. A name zypper
cannot find yet, from a `repo(...)` the same apply adds, is left to the install.

### Flatpak

```starlark
pkg("obsidian", source = flatpak("md.obsidian.Obsidian")),
pkg("mangohud", source = flatpak("org.freedesktop.Platform.VulkanLayer.MangoHud/x86_64/25.08")),
pkg("beta-app", source = flatpak("org.example.App", remote = "flathub-beta")),
```

A Flatpak installs system-wide (`flatpak install --system`) and is pinned to the commit in the
lock with `flatpak update --commit=`. A runtime, such as a Vulkan layer, takes the full
`id/arch/branch` form. Flatpak is Linux only.

### Homebrew

```starlark
pkg("wget", source = brew()),
pkg("ghostty", source = brew(cask = True)),
pkg("sketchybar", source = brew(tap = "felixkratz/formulae")),
```

`cask = True` installs a cask instead of a formula. `tap` taps a third-party repository first
and installs the item by its full name (`felixkratz/formulae/sketchybar`), which trusts that one
item of the tap and not the whole tap. The tap stays when you remove the package. Homebrew is
macOS only.

### npm

```starlark
pkg("prettier", source = npm("prettier")),
pkg("vtsls", version = "0.3.0", source = npm("@vtsls/language-server")),
```

The artifact is the registry tarball. A package without runtime `dependencies` is unpacked and
its `bin` entries are linked. A package with dependencies is installed with
`npm install --omit=dev --prefix <dir>`, so the host needs `npm` on `PATH`.

### cargo

```starlark
pkg("bat", source = cargo("bat")),
```

`cargo(name)` reads the crate's version from crates.io and its binary from the GitHub releases
of the repository the crate lists. It never compiles. A crate whose releases carry no binary for
a platform fails `fspec update` with `no prebuilt for <platform>`; use `git(...)` for it instead.

### Fixed URLs

```starlark
pkg("geist-mono", source = url("https://example.com/GeistMono.tar.xz"),
    share = {"fonts/GeistMono": "."}),
pkg("beeper", source = url("https://example.com/Beeper-4.3.73-x86_64.AppImage")),
```

`url(...)` is one file or archive at an address that does not change. `fspec update` downloads
it once and records its hash. A bare executable, such as an AppImage, becomes the command named
after the package. To move to a new version, change the URL.

A download that needs reworking before it runs takes `build`, a command that reads the unpacked
download from `$FSPEC_SRC` and installs the package into `$FSPEC_OUT`:

```starlark
pkg("beeper", source = url("https://example.com/Beeper-4.3.73-x86_64.AppImage"),
    build = ["sh", "-c", "cd $FSPEC_SRC && ./beeper --appimage-extract && cp -a squashfs-root/. $FSPEC_OUT/"]),
```

It runs on the host at every install, so keep it to unpacking and small edits; anything that
compiles belongs in a `git(...)` package. Changing the command reinstalls the package.

### Nix

```starlark
pkg("hello", source = nix("hello")),
```

On a host that has Nix, `nix(attr)` runs `nix build nixpkgs#<attr>` and links the result's
`bin/`. Nothing is hashed or copied into `/opt/fspec`. `fspec update` records the version the
attribute has in your `nixpkgs`, but the build uses whatever revision the host's `nixpkgs`
registry entry points at, so this source pins less than the others.

## Lock versions with fspec update

`fspec update` asks each source for its newest release and writes the result into `fspec.lock`
next to `spec.star`. It prints one line per change:

```console
$ fspec update
+ fd 10.5.0
+ htop 3.5.3-1
+ jq 1.7.1
+ obsidian 1.14.5
+ prettier 3.9.10
+ ripgrep 15.2.0
+ wget 1.25.0
```

A line starting with `+` is a new entry, `-` an entry no host declares any more, and
`name old -> new` a version bump. `plan` and `apply` read only the lock and never ask upstream,
so the same commit installs the same bytes on every host. Commit the lock with the spec.

To update only some packages, name them:

```sh
fspec update ripgrep fd
```

The other entries stay as they are. A name no host declares fails with
``no host declares a package named `<name>` ``.

Each entry holds an artifact for every platform the hosts that declare the package run on. A
host that applies a package the lock has no artifact for fails with
`pkg <name> has no artifact for <platform> in fspec.lock, run fspec update`. The
[lock file reference](../reference/lock-file.md) describes every field.

### Pin a version

`version` locks an exact upstream version instead of the newest:

```starlark
pkg("jq", version = "1.7.1"),
```

`version` works for aqua, github, npm, cargo, python and pypi packages. A git package pins with
its `rev` instead, and a url package with its URL.

### Hold back new releases

`release_age` on a host resolves every package to the newest release that is at least that old.
New releases then wait a few days before they reach the host, which keeps a broken or
compromised release from landing the hour it is published:

```starlark
host(name = "server", platform = "x86_64-linux", system = debian(), release_age = "3d", ...)
```

Units are `m`, `h`, `d` and `w`. When several hosts declare the same package, the longest delay
wins. `channel = "bleeding"` on a package ignores the delay for that package, and `version`
overrides it:

```starlark
pkg("claude-code", source = npm("@anthropic-ai/claude-code"), channel = "bleeding"),
```

`release_age` does not apply to distro packages. On openSUSE Tumbleweed it picks the snapshot
instead (see [Distro packages and caching](#distro-packages-and-caching)).

## Modes

| Mode | Behaviour |
| --- | --- |
| `pinned` (default) | The host always has the lock's version. A bumped lock shows as drift and the next `apply` installs the new version. |
| `self_managed` | FormalSpec installs the locked version once, then leaves the package alone. Later lock changes do not touch it. |

Use `self_managed` for software that updates itself, such as Steam, browsers or Claude Code:

```starlark
pkg("steam", source = flatpak("com.valvesoftware.Steam"), mode = "self_managed"),
```

A `self_managed` Flatpak installs without a pinned commit. Removing a `self_managed` package from
the spec still removes it.

## What gets linked

Packages from FormalSpec's own sources (aqua, github, npm, cargo, url, git) unpack into the
store, `/opt/fspec/store/<hash>-<name>-<version>/`, and their commands are linked into
`/opt/fspec/current/bin`. Put that directory on your `PATH`. The [paths reference](../reference/paths.md)
lists the rest of the layout.

FormalSpec decides which executables to link in this order:

1. The `bins` the lock entry records (the aqua registry names them, for example
   `ripgrep-15.2.0-aarch64-apple-darwin/rg`).
2. For a tree laid out as an install prefix, with `bin/` at its root, inside its single
   top-level directory, or under `usr/` (a distro package such as an Arch `.pkg.tar.zst`), the
   executables in that `bin/`. Helpers under `lib/` or `libexec/` stay
   private.
3. Otherwise, every executable regular file in the top two levels of the unpacked tree.
4. For npm, the package's `bin` entries.

`bins` on the `pkg` keeps only the commands you name, by file name. Use it when an archive holds
more executables than commands:

```starlark
pkg("discord", source = url("https://example.com/discord.tar.gz"), bins = ["Discord"]),
```

`links` adds a command name that runs one of the package's executables. The key is the new name
and the value is an executable the package ships:

```starlark
pkg("helium", source = github("imputnet/helium-linux", asset = "helium-*_linux.tar.xz"),
    links = {"helium": "helium-wrapper"}),
pkg("rustup", links = {"cargo": "rustup", "rustc": "rustup"}),
```

rustup's proxies act on the name they run as, so `cargo` linked to `rustup` behaves as `cargo`.

Each package's whole tree is also linked at `/opt/fspec/current/pkgs/<name>`. That path stays
the same across versions, so other config can name a library or data file in it by absolute
path, for example `/opt/fspec/current/pkgs/pam_ssh_agent_auth/lib/pam_ssh_agent_auth.so`.

### Fonts and themes: share

A data-only package (fonts, icon, cursor and GTK themes) has no commands. `share` maps a path
under `/usr/local/share` to a path inside the package (`.` is its root):

```starlark
pkg("bibata", version = "2.0.7",
    source = github("ful1e5/Bibata_Cursor", asset = "Bibata-Modern-Classic.tar.xz"),
    share = {"icons/Bibata-Modern-Classic": "Bibata-Modern-Classic"}),
pkg("geist-mono-nerd-font", version = "3.4.0",
    source = github("ryanoasis/nerd-fonts", asset = "GeistMono.tar.xz"),
    share = {"fonts/GeistMonoNerdFont": "."}),
```

fontconfig reads `/usr/local/share/fonts`, and icon, cursor and theme lookups walk
`XDG_DATA_DIRS`, which includes `/usr/local/share` by default. X11 cursor themes also need the
directory in `XCURSOR_PATH`. Each destination is a link into the store, replaced when the package
changes, removed with it, and reported as drift when missing. A destination must name an entry
inside a directory (`icons/<theme>`, never `icons`). `share` is Linux only.

### System config: etc

`etc` links a file the package ships into `/etc`, the same way:

```starlark
pkg("asusctl", source = github("example/asusctl"), bins = ["asusd", "asusctl"],
    etc = {"dbus-1/system.d/asusd.conf": "usr/share/dbus-1/system.d/asusd.conf"}),
```

A link under `systemd/` makes the apply run `systemctl daemon-reload` before services start, so
a `service(...)` can enable a unit the package ships. `etc` is Linux only.

### App bundles: apps

On macOS, `apps` names the `.app` bundles inside the package. Each one is copied to
`/Applications`, where Spotlight, Launchpad, the Dock and `open -a` find it:

```starlark
pkg("kopuz", version = "0.22.0", source = github("Kopuz-org/kopuz", asset = "Kopuz_*_aarch64.dmg"),
    apps = ["Kopuz.app"]),
```

It is a copy, not a link, because Spotlight skips symlinks into the store. On APFS the copy is a
clone and takes no extra space. Apply refuses to replace a bundle FormalSpec did not put there.
`apps` is macOS only.

### Apps in several archives: parts

`parts` maps a path inside the package to another `url(...)` archive, unpacked there after the
main one:

```starlark
pkg("discord", source = url("https://example.com/discord/full.distro"),
    parts = {"modules/discord_desktop_core": url("https://example.com/discord_desktop_core/1/full.distro")}),
```

`fspec update` locks each part's URL and hash. A `git` or `nix` package has no downloaded archive
and takes no `parts`.

## Distro packages and caching

Distro repositories usually keep only the newest version of a package. For dnf, pacman, apt and
xbps, `fspec update` records the exact package file URL and hash, and `apply` downloads that file
into `/var/cache/fspec/pkgs` and installs it from there. A pinned version stays installable after
the distro moves on.

How `fspec update` finds the exact file depends on the machine it runs on:

| Distro | Exact pin when |
| --- | --- |
| Arch (x86_64) | always: read from the `core` and `extra` databases on `geo.mirror.pkgbuild.com` |
| Void | always: read from `repo-default.voidlinux.org` |
| Fedora, Debian, Ubuntu | `fspec update` runs on that distro and architecture, which asks its own package manager |
| AerynOS (moss) | never: moss installs from a repository stream, and AerynOS rolls back through its own system states |
| openSUSE Tumbleweed (zypper) | never per package: the host is pinned to a dated snapshot instead |

Where no exact pin is possible, the lock holds the name only and the host installs the newest
version. A package that installs several names (`dnf([...])`) always installs by name.

Tumbleweed hosts are pinned to one of openSUSE's daily snapshots. `fspec update` moves each
Tumbleweed host to the newest snapshot (or, with `release_age`, the newest old enough) and prints
`snapshot <host> <old> -> <new>`. openSUSE keeps about a month of snapshots, so update at least
that often.

Homebrew can only install its current version. The lock records it, a different installed version
shows as drift, and `apply` runs `brew upgrade`. A rollback does not downgrade Homebrew packages.

## GitHub token and rate limit

Most sources read GitHub's API. Without a token, GitHub allows 60 requests an hour, which a spec
with a few dozen packages uses up in one `fspec update`. FormalSpec sends a token when it finds
one: `GITHUB_TOKEN`, else what `gh auth token` prints. Log in once with the GitHub CLI:

```sh
gh auth login
```

A token also lets [`gh:` loads](../concepts/hosts-and-modules.md) read private repositories.

## How it works

- Lock entries are keyed by name and source. Changing a package's source makes a new entry on
  the next `fspec update` and drops the old one.
- An artifact is checked against its `sha256-` or `sha512-` hash before anything is unpacked.
  Archive formats are recognised by their bytes: `.tar.gz`, `.tar.xz`, `.tar.zst`, `.zip`, a
  brotli-compressed tar, an Electron `.asar`, a `.dmg` on macOS, or a bare binary.
- Store directories are never rewritten. A new version gets a new directory, and the generation
  switches by renaming one symlink, `/opt/fspec/current`. See
  [How FormalSpec works](../concepts/how-it-works.md).
- Distro packages and Flatpaks live where their package manager puts them. The state file still
  records them, so `fspec rollback` reinstalls the previous set. See
  [Removal and rollback](removal-and-rollback.md).

## Troubleshooting

| Message | Fix |
| --- | --- |
| `` `<name>` is not in the aqua registry `` | The registry has no entry by that name. Use `github("owner/repo")` |
| `no asset for <os>/<arch>; candidates: ...` | The matcher found no asset for this platform. Pick one from the listed names with `github(..., asset = "glob")` |
| `` no asset matches glob `<glob>`; assets: ... `` | The `asset` glob matches nothing in the release. Compare it with the listed names |
| `<repo> has no release tagged v<version> or <version>` | The `version` you pinned is not a release tag |
| `no prebuilt for <platform>: <crate> <version> (...)` | The crate's releases carry no binary for that platform. Build it with `git(...)` |
| `version = is supported for aqua, github, npm, cargo, python and pypi packages` | Remove `version`. Pin a git package with `rev` and a url package with its URL |
| `no stable release of <repo> is old enough for the release_age policy` | Every release is newer than the host's `release_age`. Set `channel = "bleeding"`, pin a `version`, or wait |
| ``GET <url>: GitHub API rate limit reached (resets in 42 min; set GITHUB_TOKEN or run `gh auth login` for a higher limit)`` | Run `gh auth login`, or set `GITHUB_TOKEN` |
| ``fspec.lock has pkg:<name> from `<old>` but not from `<new>`; run `fspec update` `` | You changed the package's source. Run `fspec update` |
| `pkg <name> is not in fspec.lock, run fspec update` | You added a package and did not update the lock |
| `? no backend applies pkg:<name> on <host> (...)` | The source does not exist on this host: a distro source of another distro, `brew` on Linux, `flatpak` on macOS. Give the host another source |
| `? pkg:<name>: share links into /usr/local/share, which only Linux searches` | `share` on a macOS host. `apps` and `etc` fail the same way on the platform that lacks them |
| `` bins: `<name>` not among its executables (...) `` | A name in `bins` is not an executable the package ships. The message lists the ones it found |
| `it has no executables to link, no share and no apps` | The package is data only. Add `share` (Linux) or `apps` (macOS) to say where it goes |
