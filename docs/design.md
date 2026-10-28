# FormalSpec design

FormalSpec is a single Rust binary, `fspec`. It reads a config written in
Starlark, compares it with what it installed last time, and converges the
machine to match. Every version it installs is pinned in `fspec.lock`, which
lives in the same git repo as the config. Hosts that apply the same commit end
up with the same software.

## Goals

- Programmable, declarative config shared across Linux and macOS hosts.
- Identical hosts from one committed lock.
- Software available the day upstream ships it, with no central package repo
  in the way and no hashes written by hand.
- Self-updating apps keep working.
- Removing something from the config removes it from the machine, data
  included, with rollback.
- Runs on any distro; switching distros is a config change, not a USB stick.

Out of scope: building the world from source. Prebuilt upstream artifacts are
the default, and source builds happen only when a package needs patches.

## Config language: Starlark

Starlark is the Python dialect Bazel and Buck2 use for build files. FormalSpec
embeds it through the `starlark` crate, which Meta maintains for Buck2.

It won for two reasons. Agents and humans already write it fluently, since it
reads as Python. And it is deterministic by design: a config cannot read files,
call the network or look at the clock, so the same commit always evaluates to
the same plan. Lua was the runner-up; it has a bigger ecosystem, but the
determinism would have to be enforced by stripping its stdlib, and something
would eventually reach for `os.getenv`.

Host facts come from the `host()` declaration itself, never from the network,
so evaluation stays pure. `platform` is a
required argument (`"x86_64-linux"`, `"aarch64-darwin"`); `distro` and `init`
follow from `system` (`image(...)` gives the image's distro and systemd,
`fedora(...)`, `arch(...)`, `debian(...)`, `void(...)`, `aerynos(...)` and `tumbleweed(...)` give their own, and an
`aarch64-darwin` platform implies macOS and launchd). An entry in `modules` can
be a resource, a list of them, or a function, which FormalSpec calls with the
host's facts:

```python
def cli(host):
    tools = [pkg("ripgrep"), pkg("fd")]
    if host.platform.is_darwin:
        tools.append(pkg("aerospace", source = brew(cask = True)))
    return tools

hosts = [host("macbook", platform = "aarch64-darwin", modules = [cli])]
```

The host struct carries `name`, `distro`, `init` and `platform` (`name`, `os`,
`arch`, `is_darwin`, `is_linux`). `apply` probes the machine only to check that
it matches the declaration, and fails with a clear error when it does not.

`load("//modules/cli.star", "cli")` resolves from the directory holding
`spec.star`; there are no relative loads.

A recipe can live with the project it installs instead of in the spec. A
`gh:` load reads a `.star` file from a GitHub repo, pinned like a package:

```python
load("gh:FormalStudio/formalmusic@main//fspec.star", "formalmusic")
hosts = [host("macbook", ..., modules = [[formalmusic()]])]
```

`@<branch>` or `@<tag>` tracks (each `fspec update` moves it), `@<commit>`
(40 hex) pins, and no `@` means the default branch. `fspec update` resolves the
ref with `git ls-remote`, fetches the file and every `//x.star` it loads, which
resolve inside that repo at that commit, and records them in `fspec.lock`:

```toml
[loads."gh:FormalStudio/formalmusic@main"]
rev = "0123456789abcdef0123456789abcdef01234567"
files = { "fspec.star" = "sha256-...", "lib.star" = "sha256-..." }
```

`fspec update <pkg>` keeps every load at its locked commit. Evaluation never
resolves a ref: it reads each file at the locked commit from
`<spec>/.fspec/loads`, then `~/.cache/fspec/loads`, then GitHub's contents API
(with the same token as the resolvers, so private repos work), and checks it
against the locked hash. A load the lock lacks, or a file that does not match
it, fails with `run fspec update`. Inside such a file `origin()` returns
`struct(url = "https://github.com/<owner>/<repo>", rev = "<locked commit>")`,
also inside a function the spec calls, and `None` in the spec's own files, so
the project's recipe can build itself at the commit it was read from:

```python
def formalmusic():
    o = origin()
    return pkg("formalmusic", source = git(o.url, rev = o.rev), build_flags = [...])
```

One lock entry then pins, or tracks, both the recipe and the build. A loaded
file names paths relative to the user's spec (`glob`, `src`, `patches`), not to
its own repo.

## Resources

A config evaluates to a list of resources:

| Resource | What it is |
| --- | --- |
| `host` | Name, `platform`, `system`, users, modules, and `locale`, `keymap` and `timezone` |
| `pkg` | A package from some source, pinned in the lock, with the files, services and timers that belong to it |
| `file` | A file FormalSpec writes, links, seeds or patches |
| `service` | A system or user service, rendered for the host's init |
| `user` | Account, UID, shell, groups |
| `secret` | An age-encrypted file decrypted at apply |
| `defaults` | A macOS `defaults` key, `system = True` for /Library/Preferences |
| `setting` | A macOS system setting behind `pmset`, `systemsetup` or the firewall |
| `image` | A bootc base image for the system layer |
| `timer` | A scheduled job (systemd timer, cron, launchd) |

Modules are plain Starlark functions that return lists of resources.

A package names the things that exist only for it, and they go when it goes:

```python
pkg("syncthing", source = github("syncthing/syncthing"),
    services = [service("syncthing", exec = "syncthing serve", user = True)],
    files = [file("~/.config/syncthing/config.xml", src = "files/syncthing.xml")])
```

A host's `locale`, `keymap` and `timezone` apply as one `locale` resource on
Linux. fspec writes the files `localectl` and `timedatectl` would, since an
install runs the first apply in a chroot where neither works: `LANG` in
`/etc/locale.conf` (`/etc/default/locale` on Debian and Ubuntu), `KEYMAP` and
`XKBLAYOUT` in `/etc/vconsole.conf` (`KEYMAP` in `/etc/rc.conf` on Void,
`XKBLAYOUT` also in `/etc/default/keyboard` on Debian and Ubuntu), and
`/etc/localtime` as a link into `/usr/share/zoneinfo`. A changed locale is
then built the distro's way: `locale-gen` from `/etc/locale.gen` on Arch and
Debian, `locale-gen <locale>` on Ubuntu, `xbps-reconfigure -f glibc-locales`
from `/etc/default/libc-locales` on Void, and `glibc-langpack-<lang>` on
Fedora. Removing the fields leaves the files as they are.

A `file`, `service` or `timer` declared on its own is its own resource and is
removed when its own line disappears. Two declarations of the same file path,
package name or service name fail the plan.

## Package sources

| Source | Example |
| --- | --- |
| aqua registry (default for CLI tools) | `pkg("ripgrep")` |
| `github(repo)` | release assets matched per platform, or picked by an `asset = "tool-*-x86_64.pkg.tar.zst"` glob, which may name a distro package |
| `npm(name)` | `npm("@anthropic-ai/claude-code")` |
| `cargo(name)` | crates.io, prebuilt via cargo-binstall where available |
| `flatpak(id)` | Flathub, `id/arch/branch` for a runtime such as a Vulkan layer |
| `brew(name)` | Homebrew bottles and casks on macOS, `tap = "owner/repo"` for a third-party tap |
| `dnf`, `pacman`, `apt`, `xbps`, `moss`, `zypper` | the distro's own repos, plus `repo(...)` ones on zypper |
| `git(url, rev)` | build from source with a recipe |
| `nix(attr)` | borrow a nixpkgs build on hosts that have Nix |

`rev = "main"` and `"latest"` are resolved by `fspec update`, never at apply,
so the lock always holds an exact commit or version.

Resolvers read GitHub's API with a token when one is available: `GITHUB_TOKEN`,
else what `gh auth token` prints. Without one GitHub allows 60 requests an hour,
and a refusal for that reason is reported as the rate limit (with when it
resets), never as a missing release.

### Modes

- `pinned` (default): exact version and hash from the lock.
- `self_managed`: FormalSpec pins and installs the first version, then leaves
  the app's own updater alone. For Steam, Claude Code, browsers.
- `channel = "bleeding"` on a package skips any release-age delay a host
  sets as policy (`host(release_age = "3d")`). A package resolves to the newest
  release at least that old, using the longest delay among the hosts that
  declare it.

### Patches and build flags

A package with patches moves from prebuilt to source build:

```python
pkg("librepods",
    source = git("https://github.com/…/librepods"),
    patches = glob("patches/librepods-*.patch"),
    build_flags = ["--features", "pipewire"])
```

`glob(pattern)` returns the sorted, repo-relative paths of the files under the
spec's directory that match. It is the one place evaluation reads the
filesystem besides `load`, and it only reads the spec's own tree: patterns
cannot start with `/` or contain `..`, and links that lead outside the tree are
dropped.

A branch `rev` locks the branch's tip, unless `git(..., paths = [...])` names
the files or directories that make the build: then `fspec update` takes a bare
`--filter=blob:none` clone of the branch and locks
`git log -1 <tip> -- <paths>`, so commits that touch nothing else leave the
lock and its cached build alone. Tags and full commits ignore `paths`.

`fspec update` writes `patches = "sha256-…"` into the lock: a hash over the
patch file names and contents, in the order the spec lists them. A package
whose patches no longer hash to the lock's value is refused ("patches changed,
run fspec update"), at `apply` and as a drift note in `plan`.

Fields on `pkg` for source builds: `patches`, `build_flags` (appended to the
recipe's configure or build command), `build = ["sh", "-c", "…"]` (replaces
recipe detection) and `bins = ["name", …]` (the executables to ship and link,
by file name; default is every executable the recipe produced). `bins` works the
same on prebuilt archives: it picks among the executables the store finds.

**Recipes.** `fspec build` checks out the locked commit, applies the patches
with `git apply`, then picks the first match: `Cargo.toml`, `meson.build`,
`CMakeLists.txt`, `go.mod`, a `Makefile`. An explicit `build` wins over all of
them.

| Recipe | Commands |
| --- | --- |
| Cargo | `cargo build --release --locked --manifest-path <src>/Cargo.toml --target-dir <build> <build_flags>`; executables come from `<build>/release` |
| Meson | `meson setup <build> <src> --buildtype release --prefix <out> <flags>`, `meson compile -C <build>`, `meson install -C <build>` |
| CMake | `cmake -S <src> -B <build> -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=<out> <flags>`, `cmake --build <build> --parallel`, `cmake --install <build>` |
| Go | `go build -C <src> -o <out>/bin/ <flags> ./...` |
| Make | `make -C <src> <flags>`, `make -C <src> install PREFIX=<out>` |
| explicit | the `build` command, with `FSPEC_SRC` (the patched checkout), `FSPEC_OUT` (put executables in `$FSPEC_OUT/bin`) and `FSPEC_CACHE` (a directory kept between builds, for toolchains and target dirs) in its environment; `build_flags` do not apply |

**Build cache.** `<src>`, `<build>` and `FSPEC_CACHE` live in
`~/.cache/fspec/build/<name>-<platform>/` on the machine that builds, and stay
there between revs: the next rev fetches into the same checkout (a forced
checkout and `git clean -ffd` drop the last build's patches and untracked
files, ignored ones stay), so Cargo, Meson and CMake build incrementally. A
second build of the same package waits on `<name>-<platform>.lock` beside it.
Only `<out>` is new each time. With docker, whose root is the host's root, the
container hands the mounted directories back to the building user when it
exits.

Every command goes through the `Runner`. Non-Cargo recipes install into
`<out>`, a prefix, and the artifact is that whole tree (directories, modes and
symlinks kept), so data in `share/` and helpers in `lib/` ship with the
executables in `bin/`. A Cargo build ships the executables of `<build>/release`
at the top.

**Build dependencies.** A runner has only what its image ships, and the
binary has to load on the host, so a `git` source can name the image its build
runs in and the packages the build needs there:

```python
pkg("librepods", source = git("https://github.com/…/omarchy-pods", rev = "…",
    image = "registry.opensuse.org/opensuse/tumbleweed",
    deps = ["cmake", "gcc-c++", "qt6-base-devel", "libpulse-devel"]))
```

With `image` set, `fspec build` still checks out and patches on the build
machine, then runs the whole recipe as one `podman run --rm` (`docker run`
where podman is missing) of that image, with the work directory and the build cache mounted at their
own paths and `FSPEC_SRC`, `FSPEC_OUT` and `FSPEC_CACHE` passed in. `deps` install first, by
whichever of zypper, dnf, apt-get, pacman, xbps-install or apk the image has,
so they are named the way that distro names them (`pkgconfig(gtk4)` works on
zypper and dnf). Each manager downloads into `pkgs/` in the package's build
cache and keeps the files (zypper `mr -k`, dnf `keepcache`, apt with docker's
`docker-clean` removed), so a warm build fetches only what changed. Building in the host's own distro links against the same
sonames the host has. On Linux, `deps` without `image` fails the plan. A
macOS build has no container of its kind, so there `image` fails the plan and
`deps` are Homebrew formulae, installed with `brew install` before the recipe
runs (`git(..., deps = ["go"])`); the CI's macOS runners have brew. Pin the image by
digest (`…@sha256:…`) for builds that repeat exactly. The CI workflow needs no
change: GitHub's Ubuntu runners have podman.

**Commands.**

- `fspec build --pkg <name> --platform <p> --out <dir>` builds on this
  machine, which must be platform `<p>`. It writes
  `<dir>/<name>-<p>.tar.zst` with the executables at the top (sorted names,
  zeroed owners and times) and `<dir>/tag` holding `<rev>-<p>`. With
  `--host <h>` it builds on that host's builders first (below). When one
  name is `git(...)` on one host and another source elsewhere, it builds the
  git one, preferring a host of platform `<p>`.
- `fspec ci packages` writes `.github/workflows/fspec-packages.yml` next to the
  spec: a matrix with one job per `git(...)` package and platform that a host
  declares (`ubuntu-24.04` for x86_64-linux, `ubuntu-24.04-arm` for
  aarch64-linux, `macos-26` for aarch64-darwin). Each job installs fspec and
  oras, runs `fspec build` and pushes with `oras push`. A `record` job then
  runs `fspec update --cache` on the branch and commits the lock with
  GITHUB_TOKEN, whose push starts no workflow; pull requests skip it.
  Regenerate it when the
  set of packages or platforms changes; the lock and `patches/` changes on the
  branch checked out when it was written (main when HEAD is detached) trigger
  it, so a spec kept on its own branch rebuilds from that branch.
- `fspec update --cache [--registry r]` resolves no versions, only
  `<registry>/<name>:<rev>-<platform>` with
  `oras manifest fetch --descriptor` and records
  `<platform> = { oci = "<registry>/<name>@sha256:…" }`. The registry defaults
  to `ghcr.io/<origin owner>/fspec-cache`. A plain `fspec update` keeps the
  recorded digests while the commit and the patches are unchanged.

**Registry layout.** One repository per package,
`ghcr.io/<owner>/fspec-cache/<name>`, one tag per commit and platform
(`<rev>-<platform>`), each a one-layer artifact whose layer is the `.tar.zst`
(media type `application/vnd.fspec.package.v1.tar+zstd`, file name as its
title). The lock pins the manifest digest.

**On the host.** A package whose locked artifact has `oci` is pulled with
`oras pull <ref> -o <staging>` through the `Runner` (so `oras` must be on
PATH), the layer file is hashed against the layer digest in
`oras manifest fetch <ref>`, unpacked, and installed like any other store
package. A reference without a digest is refused.

**Builders.** A `git` package the lock has no `oci` artifact for (CI has not
built it, or nobody runs CI for it) is built instead of failing the apply. A
host names the machines that build for it:

```python
host(name = "e1504g", platform = "x86_64-linux",
    builders = [builder("g815", routes = ["g815", "192.168.86.20"])])
```

`routes` are ssh destinations for the same machine, in order of preference
(the Tailscale name, then the LAN address); without them the name is the
route. The first time a run needs a builder it probes the routes in order with
`ssh -o ConnectTimeout=5 -o BatchMode=yes -o ServerAliveInterval=15 <route> uname -sm` and keeps the
first that answers for the rest of the run, so a builder that is down costs
one timeout per route per run, not one per package. A builder of another
platform is skipped. On the builder, fspec ships itself, a job file (the
package as the spec declares it, the locked rev) and the package's patches to
`~/.cache/fspec/jobs/<name>-<rev>-<id>`, its own directory per job, runs
`fspec build --job` there, copies the `.tar.zst` back over ssh and deletes the
directory. The builder's build cache is what carries between jobs. Nothing is
installed on the builder, which needs only git, ssh and the recipe's tools
(podman or docker for `image`). Run as root through sudo, the ssh runs as
`SUDO_USER`, whose keys reach the builder. When every builder is unreachable
or fails, the apply fails with each builder's reason: a host that names
builders is one that should not compile. The order is the CI artifact, then
builders in the order listed; only a host without builders builds itself.

The store directory of a built package hashes the url, the rev, the patches
and the recipe (`build`, `build_flags`, `image`, `deps`), so a changed recipe
builds anew. Builders exist for software no distro packages and no release ships,
like FormalShell or a project the user compiles themselves; everything else
comes from the distro or from a prebuilt release.

Patches on npm and other packages that ship as an unpacked tree need no build:
`prepare` applies them in pure Rust (the `diffy` crate: git or unified diffs
with `a/` and `b/` prefixes, text edits, file creation, deletion and rename; a
rename with no hunks moves the file as is, so it works on binaries)
to the package directory (`package/` for npm, `node_modules/<name>` after an
`npm install`, else the archive's single top-level directory) before the tree
gets its store path. The store directory name hashes the artifact hash together
with the patches hash, so a patched tree never shares a path with the
unpatched one.

## The lock

`fspec.lock` is TOML, generated by `fspec update` and committed:

```toml
[[pkg]]
name = "ripgrep"
source = "aqua:BurntSushi/ripgrep"
version = "15.1.0"
x86_64-linux = { url = "https://github.com/BurntSushi/ripgrep/releases/…", hash = "sha256-…" }
aarch64-darwin = { url = "https://github.com/BurntSushi/ripgrep/releases/…", hash = "sha256-…" }

[[pkg]]
name = "librepods"
source = "git:https://github.com/…/librepods"
rev = "4f1c2a9…"
patches = "sha256-…"
x86_64-linux = { oci = "ghcr.io/formalstudio/fspec-cache/librepods@sha256:…" }

[system.g815]
image = "ghcr.io/ublue-os/bazzite-nvidia@sha256:…"

[zypper.e1504g]
snapshot = "20261020"
```

Distro repos usually keep only the newest version of a package, so FormalSpec
caches the exact `.rpm`, `.pkg.tar.zst` or `.xbps` files it locked. A pin
stays installable after the distro moves on.

`fspec update [pkg...]` resolves every package declared by any host (or only
the named ones), keeps the other entries, drops entries no host declares and
prints one line per change. Entries are keyed by name and source, so hosts can
take one name from different sources (`brew` on the macbook, `github` on
Linux) and each reads its own entry. A package gets an artifact for each platform its
hosts declare. A distro source (`dnf`, `pacman`, `apt`, `xbps`) records the
version plus the exact package file URL and hash per platform, found two ways.
When the machine running `fspec update` is that distro on that arch, it asks its
own package manager: `dnf repoquery --location`, `pacman -Sddp`,
`apt-get download --print-uris`, `xbps-query -R --property=...`. Otherwise
Arch reads the `core.db` and `extra.db` tarballs from `geo.mirror.pkgbuild.com`
(x86_64 only; Arch Linux ARM has its own mirrors) and Void reads
`<arch>-repodata` from `repo-default.voidlinux.org`. Fedora and Debian have no
repo metadata simple enough to parse here, so on any other machine those
platforms stay name-only: the host installs the newest version by name and
nothing is cached. Run `fspec update` on a Fedora or Debian machine of the
right arch to get exact pins; `release_age` does not apply to distro sources.
`moss` packages (AerynOS) always lock by name only. moss installs from a
repository stream rather than from package files, so there is no file to
cache, and AerynOS's own reproducibility unit is the whole-system state moss
records on every transaction; a rollback goes back to that state (see the
system layer).
`zypper` packages (openSUSE Tumbleweed) lock by name too. What pins their
versions is the snapshot in `[zypper.<host>]`: openSUSE publishes every
Tumbleweed snapshot under `https://download.opensuse.org/history/<YYYYMMDD>/tumbleweed/`
and lists them newest first in `https://download.opensuse.org/history/list`.
`fspec update` (without package names) pins each Tumbleweed host to the newest
listed snapshot, or with `release_age` the newest whose day ended at least that
long ago, and prints `snapshot <host> <old> -> <new>`. openSUSE keeps about a
month of snapshots, so a pin older than that stops resolving and needs an
update.
The lock entry for a distro package holds one artifact per platform, so the
cached file is used for single-name packages; a multi-name `dnf([...])` package
installs by name. Flatpak pins live in `any = { commit = … }`, read over HTTP
from the OSTree repo's `refs/heads` for flathub and flathub-beta, so a host
without flatpak (the macbook) locks every Linux host's flatpaks; another remote
is asked through the local `flatpak remote-info`. Brew records
`version`; Homebrew can only install the current version, so the brew pin is
checked (drift, then `brew upgrade`), not enforced, and an upgrade is not
reversed on rollback.
A `tap` is tapped before the install, and the package is installed by its full
name (`felixkratz/formulae/sketchybar`), which trusts that one item of the tap
rather than the whole tap. The tap stays when the package is removed. A tapped
package has no version until it is installed: formulae.brew.sh only knows
Homebrew's own taps.

A nightly CI job can run `fspec update` and open a PR with each bump and a
changelog link. Merging it is the auto-update.

## Store and generations

Packages from FormalSpec's own sources unpack into
`/opt/fspec/store/<hash>-<name>-<version>/`, never into `/usr`. A generation is
a directory of symlinks into the store, and `/opt/fspec/current` points at the
active one. On macOS the root is `/opt/fspec` too.

This gives FormalSpec three things Nix users expect:

- Switching is a single `rename(2)` of `current`, so it is atomic.
- Two versions of a tool can coexist; a project picks one in its `dev.star`
  (see [Project environments](#project-environments)).
- `fspec rollback` repoints `current` at the previous generation.

Users put `/opt/fspec/current/bin` on PATH; nothing else exposes store tools.
`/opt/fspec/current/pkgs/<name>` links each package's whole tree, the stable
path for a library or data file that other config names by absolute path, on
every platform: `/opt/fspec/current/pkgs/pam_ssh_agent_auth/lib/pam_ssh_agent_auth.so`
in `/etc/pam.d/sudo_local`, or SbarLua's `sketchybar.so` in a `LUA_CPATH`. A
source build may install only such files; a download with no executables and
no `share` is still refused as a wrong artifact.

A generation is `generations/<n>/bin/`, symlinks into the store (relative, so a
rooted tree stays movable). Each apply starts from the links of the generation
`current` points at, applies only the changed packages, and renames a temp
symlink over `current` last; the journal restores the old link if anything
fails. Which executables get linked: the lock artifact's `bins` when set; else,
for a tree laid out as an install prefix (`bin/` at its root or in its single
top-level directory, as FormalShell's `formalshell/bin/` and terminal-browser's
`terminal-browser/bin/`, or `usr/bin/` for a distro package such as asusctl's
Arch `.pkg.tar.zst`), the executables in that `bin/`; else every executable
regular file in the top two levels of the unpacked tree; for npm, the package's
`bin` entries. A prefix keeps its private helpers in `lib/` or `libexec/`
(FormalShell ships its own `matugen` there), so searching it deeper would link
them over the real commands; `bin/` is where a prefix puts what it means to
expose. `bins = ["name", …]` on the `pkg` then keeps only those commands, for an
archive whose top levels hold more executables than commands (Discord's
`files/` also holds `chrome_crashpad_handler`). Store directories are named
`<hash8>-<name>-<version>`, never rewritten, and reused when present. An
artifact is verified against its `sha256-` or `sha512-` SRI hash before
anything is unpacked (`.tar.gz`, `.tar.xz`, `.tar.zst`, `.zip`, a
brotli-compressed tar, an Electron `.asar`, a `.dmg` on macOS, or a bare
binary named after the package). Formats are told apart by their bytes, not their names. Brotli has no
magic number, so a download that matches nothing else is decoded and kept as a
tar only when the result is one (`ustar` at offset 257): Discord publishes its
Linux builds that way (`full.distro`). An asar is unpacked the way moonlight's
own extractor reads it, refusing entries that name `..` or live outside the
archive; moonlight's extensions ship as asars. A `.dmg` (its `koly` trailer) is
mounted read-only with `hdiutil` and its top level copied out with `ditto`,
leaving out the `Applications` link and hidden files of a drag-to-install
window. Release matching skips `.dmg` assets unless an `asset` glob names one.

Data-only packages (fonts, icon and cursor themes, GTK themes) have no
executables; they declare where their trees go with `share`, a map from a path
under `/usr/local/share` to a path inside the package (`.` for its root):

```python
pkg("bibata", source = url(".../Bibata-Modern-Classic.tar.xz"),
    share = {"icons/Bibata-Modern-Classic": "Bibata-Modern-Classic"})
pkg("geist-mono-nerd-font", source = url(".../GeistMono.tar.xz"),
    share = {"fonts/GeistMonoNerdFont": "."})
```

Store packages belong to the system scope, so the links go where every user's
lookup reaches: fontconfig reads `/usr/local/share/fonts`, and icon, cursor and
GTK theme lookups walk `XDG_DATA_DIRS`, which defaults to
`/usr/local/share:/usr/share` (libXcursor's own default path skips
`/usr/local/share/icons`, so an X11 cursor theme also needs it in
`XCURSOR_PATH`). Each link points straight at the store path and is replaced
when the package changes, removed with it, and reported as drift when missing.
A destination must sit inside a directory (`icons/<theme>`, never `icons`), and
apply refuses to replace anything there that is not a link. A package with
neither executables, `share` nor `apps` is refused. `share` is Linux only.

A package's own system config goes under `/etc` with `etc`, the same kind of
map from a path under `/etc` to a path inside the package:

```python
pkg("asusctl", source = github(...),
    etc = {"dbus-1/system.d/asusd.conf": "usr/share/dbus-1/system.d/asusd.conf"})
```

The links behave like `share` links (replaced on change, removed with the
package, drift when missing, never over a file fspec did not link). A link
under `systemd/` makes the store run `systemctl daemon-reload` before the
services phase, so `service("asusd")` can enable a unit the package ships. That
suits a unit whose `ExecStart` already names a path that exists; an upstream
unit that runs `/usr/bin/asusd` needs a `service(exec = ...)` with the store
path instead. `etc` is Linux only.

An app that ships as several archives takes the rest as `parts`, a map from a
path inside the package to a `url(...)`, each unpacked there after the main
archive and before patches:

```python
pkg("discord", source = url(".../discord-1.0.160.distro"),
    parts = {"modules/discord_desktop_core": url(".../discord_desktop_core-1.0.distro")})
```

Paths are relative to the package root, the archive's single top-level
directory when it has one (Discord's `files/`), as for patches. `fspec update`
locks each part's url and hash under the package's `parts`; a part whose url
the lock does not have fails the apply until the next update. The parts'
hashes are part of the store directory's name. A `git` or `nix` package has no
downloaded archive and takes no `parts`.

A `url(...)` package can also take `build`, a command run on the host after the
parts and patches, for a download that needs more than a text patch (an
AppImage whose bundled app has to be extracted and edited). It sees the
unpacked download in `FSPEC_SRC` and installs the package into `FSPEC_OUT`,
which becomes the store directory; a hash of the command is part of that
directory's name, so editing it reinstalls. It runs at every install, with no
build cache and no builder, so it suits seconds of unpacking, not a compile.
`build` on any other downloaded source is refused.

macOS app bundles go where Spotlight, Launchpad, the Dock and `open -a` find
them with `apps`, the bundle paths inside the package:

```python
pkg("kopuz", source = github("Kopuz-org/kopuz", asset = "*.dmg"), apps = ["Kopuz.app"])
```

Each bundle is copied to `/Applications/<its name>`, because Spotlight and
Launchpad skip a symlink into the store (the reason nix-darwin users reach for
mac-app-util). On APFS the copy is a clone, so it shares the store's blocks and
costs no space. The copy is replaced when the package changes, removed with it,
and reported as drift when missing; apply refuses to replace a bundle that fspec
did not put there. `apps` is macOS only.

Each pkg's record keeps the store path, version, linked bins and the lock entry
it was installed from. `fspec rollback [gen]` re-applies that generation's
resources with the lock's packages rebuilt from those records, as a new
generation, so the old store paths are reused and no download happens. The
rest of the lock (the Tumbleweed snapshot, image digests) is the live one's,
since a generation records only its packages. The live `fspec.lock` is
untouched: until it is reverted too, `plan` reports the newer locked version as
drift. A `pinned` pkg whose locked artifact differs from the record is drift
(that is how a bumped lock reaches `apply`); a `self_managed` pkg is installed
once and never touched by later lock changes.

npm packages are the registry tarball. One without runtime `dependencies` is
unpacked and its `bin` linked; one with dependencies is installed with
`npm install --omit=dev --prefix <dir>`, so the host needs npm on PATH. The
`nix` source runs `nix build --no-link --print-out-paths nixpkgs#<attr>` (pinned
to the lock's `rev` when set) and links the result's `bin/`; nothing is hashed or
copied.

Distro packages and Flatpaks live where their package manager puts them; the
state file still records them, so rollback reinstalls the previous set.

## Project environments

A `dev.star` at a repo root is FormalSpec's version of a flake devShell: the
tools, system libraries, environment and services a project needs, locked so
that everyone who clones the repo gets the same versions.

```python
dev(
    packages = [pkg("bun", version = "1.3.1"), pkg("just")],
    libs = [lib("openssl"), lib("gtk4")],
    bins = [bin("cmake")],
    env = {"RUST_LOG": "info"},
    services = [service("postgres", version = "17")],
    shell_hook = "export DATABASE_URL=postgres://localhost/app",
    container = None,
)
```

`dev.star` is evaluated once per platform with a `platform` global (`name`,
`os`, `arch`, `is_darwin`, `is_linux`), so a file branches the way a flake does
on `stdenv.isLinux`. `platforms = [...]` narrows the four defaults.

**Packages** are ordinary `pkg(...)` resources from FormalSpec's own sources.
`version =` pins an exact upstream version instead of the newest release, and
`links = {"cargo": "rustup"}` adds a command that runs another one under a new
name (rustup's proxies act on the name they run as). Three sources exist for
projects in particular:

- `pkg("python", version = "3.13")` is CPython from python-build-standalone,
  at the build and SHA-256 uv's own download metadata names for that version
  and platform (the aqua registry carries no CPython).
- `pypi("yt-dlp")` is a PyPI distribution. `fspec update` has uv compile it and
  its dependencies for every platform at once (`uv pip compile --universal
  --generate-hashes`, for the locked python's version) and keeps the hashed
  requirement lines in the lock; activation installs exactly those into a
  relocatable virtualenv over the project's python, in the user store. uv is
  added as a package when `dev.star` does not name it. The venv is built for
  the machine, so a pypi package does not run in a container yet.
- `url("https://...")` is one file or archive at a fixed URL, pinned by its
  hash. An ELF executable (an AppImage is one) becomes the command named after
  the package. Any other file that is not an archive keeps its name and is
  data, not a command: fonts, a model, a fixture.

Each package's tree is exported as `FSPEC_PKG_<NAME>` (`FSPEC_PKG_YT_DLP`), so
`env` and `shell_hook` can point into it: a venv's interpreter, a font
directory for a generated `fonts.conf`.

**Locking.** `fspec update` in a directory with a `dev.star` and no `spec.star`
writes `fspec.lock` next to it: every package's artifacts for every platform,
the digest of the container and of each service image, and the package that
provides each `lib()` and `bin()` on each package manager. Activation reads the
lock and never resolves anything.

**Activation** puts the project's packages on PATH and sets its environment;
nothing is installed globally. Each package is the same side-by-side store
directory a host uses: `/opt/fspec/store/<dirname>` when the host already has
it, else unpacked into the user's store at `~/.local/share/fspec/store`. The
commands are linked from `~/.cache/fspec/dev/farms/<hash>/bin`, put first on
PATH, then `env` is set and `shell_hook` runs in `sh`; the variables it exports
are what the shell gets.

- `fspec dev` opens `$SHELL` in the environment; `fspec dev -- cargo build`
  runs one command.
- `use fspec` in an `.envrc` mirrors `use flake`. `fspec dev direnv` prints
  the direnv library that defines it, for `~/.config/direnv/lib/fspec.sh`; it
  evaluates `fspec dev export`, the bash that turns the calling environment
  into the project's, and reloads when `dev.star` or `fspec.lock` changes.
- `fspec dev plan` shows what the environment holds and every dependency the
  lock has no package for.

### Native libraries and tools

`lib("gtk4")` is the pkg-config module `gtk4`, and `bin("cmake")` an
executable. FormalSpec keeps no table of package names: at `fspec update` it
asks each package manager what provides the file, and writes the answer into
the lock.

| Manager | `lib("gtk4")` | `bin("cmake")` |
| --- | --- | --- |
| dnf | installs `pkgconfig(gtk4)` | installs `/usr/bin/cmake` |
| zypper | installs `pkgconfig(gtk4)` | `zypper se --file-list` |
| moss | installs `pkgconfig(gtk4)` | installs `binary(cmake)` |
| apt | `apt-file search` for `gtk4.pc` | `apt-file search` for `bin/cmake` |
| pacman | `pacman -F gtk4.pc` | `pacman -F cmake` |
| xbps | `xlocate` | `xlocate` |
| brew | the formula whose bottle ships `lib/pkgconfig/gtk4.pc` | the formula listing `cmake` in `executables` |

A name ending in `.so` is a shared object under the library directory instead:
`lib("gstreamer-1.0/libgstlibav.so")` for a plugin that is loaded at runtime
and ships no pkg-config module. dnf installs it by path (`/usr/lib64/...`),
zypper by the soname every RPM provides (`libgstlibav.so()(64bit)`, since its
file index lists only bin and etc), apt, pacman and xbps find it in their file
index, and moss, which installs by provider only, records a miss.

dnf, zypper and moss resolve capabilities themselves, so the lock holds the
capability. The file-index lookups run in a container of that distro (debian,
archlinux, void, tumbleweed), so `fspec update` needs docker or podman for
them. brew's API lists no files, so a pkg-config module is matched to formulae
named after it (`dbus-1` checks `dbus`) and confirmed against the bottle's
contents. When several packages ship the file, the one with the same name (for
a bin) or the only `-dev`/`-devel` one (for a lib) wins; otherwise it is a
miss. xlocate's index lags behind Void's repository, so an executable it does
not know is checked against the file list of the package with the same name. A
miss is recorded in the lock with its reason, `fspec update` and `fspec dev
plan` print it, and `lib("x", apt = "libx-dev")` names the package by hand for
the lookups that cannot resolve it. A lookup that cannot run at all (no
network, a dropped docker connection) is not a miss: the lock keeps what it
had for that manager and `fspec update` fails, so a flaky run never writes
misses that are not true.

Activation installs nothing system-wide. It checks that each module's `.pc` or
each executable is present (on macOS, that `$(brew --prefix)/opt/<formula>`
exists, whose `lib/pkgconfig`, `lib`, `include` and `bin` go on
`PKG_CONFIG_PATH`, `LIBRARY_PATH`, `CPATH` and PATH) and fails with the install
command built from the locked names when something is missing.

A machine whose package manager has a miss for some dependency (Void has no
libsystemd), or that has no package manager fspec knows (NixOS), cannot
satisfy `lib()` and `bin()` natively. With `fallback_container = "debian:13"`
the shell runs in that image instead, with a line saying which dependencies
sent it there; without it, activation fails and names the misses. `fspec dev
plan` shows the same either way. Nothing is skipped silently.

`platforms = [...]` limits a project to some platforms. On any other, `fspec
dev plan` says the project does not support this machine instead of failing,
and `fspec dev` refuses to start.

`container = "debian:13"` is the reproducible option: the shell runs in the
pinned image plus one derived layer that installs the locked packages from that
distro's repositories, tagged by a hash of its Containerfile. The project, a
per-project home and the store are mounted at the paths they have outside, with
the host network and the caller's uid. On macOS the container runs the
`<arch>-linux` evaluation of `dev.star`.

This is weaker than Nix. A Nix devShell pins every library to an exact store
path built from a hashed recipe, so two machines get bit-identical inputs.
FormalSpec pins its own packages that way, but `lib()` and `bin()` pin only a
package name: the version is whatever the distro, or brew, ships that day, and
two developers on Fedora and Arch build against different gtk4 releases. The
container narrows this to one distro and one base digest, yet its `apt-get
install` still takes the repository's current versions when the layer is
built. Making it exact would mean either a package set FormalSpec builds itself
(which is Nix) or snapshot mirrors for every distro, and both are out of scope.

### Services

`service("postgres", version = "17")` without `exec` runs the image
`postgres:17`, pinned by digest in the lock, detached with each exposed port
published on 127.0.0.1 and a named volume per declared volume, so data
survives restarts. With `exec = "..."` it is a process started from the
project's environment in its own process group, logging under
`~/.cache/fspec/dev/run/<project>/logs`. Services start when the first
`fspec dev` shell of the project opens and stop when the last one exits;
`fspec dev up` starts them and keeps them running until `fspec dev down`. A
direnv shell does not start services, since direnv has no exit hook.

`fspec gc` also drops link farms whose project is gone or that were not used
within the cutoff, then the user-store packages no remaining farm links into.

## Apply is a transaction

1. Fetch every artifact into the store and verify hashes. Nothing on the live
   system has changed yet, so a network failure aborts cleanly.
2. Render every file into a staging directory.
3. Write a journal of the intended changes.
4. Apply: files by write-then-rename, generation switch, package manager
   operations, then service reloads last.
5. On failure, replay the journal backwards. A crash mid-apply is resumed or
   reverted from the journal on the next run.

Each scope's apply holds `<state dir>/apply.lock` from before that recovery to
the end, with its pid in the file. A second apply of the same scope is refused
and names the pid, rather than replaying the first one's live journal or
sharing its staging directory.

Before step 4, FormalSpec takes a btrfs or APFS snapshot of the paths it is
about to touch when the filesystem supports it.

## State

`/var/lib/fspec/state.json` (system) and `~/.local/state/fspec/state.json`
(user) record everything FormalSpec owns: each package and where it went, each
file with the hash it wrote, each service, each app data path it attributed to
a package, and the last-applied value of every key in a patched file. Plan is a
diff between the evaluated config and this state.

## Removal and cleanup

Deleting a line from the config removes the thing completely on the next
apply, the way removing a package from a NixOS config does, and goes further:
app data goes too.

When a `pkg` disappears from the config, apply:

1. Stops and removes the services and timers declared on it
   (`pkg(services = [...], timers = [...])`).
2. Uninstalls it (drops it from the generation, or asks the package manager).
3. Deletes the files declared on it (`pkg(files = [...])`), if they still match
   the hash it wrote. A file you or the app edited is kept and reported. Which
   module returned a file never ties it to a package; only `files = [...]` does.
4. Moves its app data to the generation trash.

App data is found three ways, in order:

- Declared on the package: `pkg("steam", data = ["~/.local/share/Steam"])`.
- Known by the source: Flatpak data under `~/.var/app/<id>`, registry metadata
  listing XDG paths for common apps.
- Attributed at install: after a package's first run, FormalSpec records new
  entries that appeared under `~/.config`, `~/.local/share`, `~/.local/state`
  and `~/.cache` whose names match the package. Anything it cannot attribute
  shows up in `fspec orphans` for you to claim or delete.

Trash lives under `/var/lib/fspec/trash/<generation>/` and
`~/.local/state/fspec/trash/<generation>/`, chosen by where the path lives, with a
`manifest.json` per generation that remembers each item's package and original
path. When the system scope trashes a path in the home (Flatpak data), the
directory is named `system-<generation>`, so it never collides with the user
scope's own numbering. The engine does the moving, after every backend has
removed the package and under the apply's journal, so every package source
behaves the same and a failing apply puts the data back. Plan lists the paths
that will move on the package's removal line (`data to trash: ...`).

A package that returns, whether through `fspec rollback` or a plain re-add,
gets back the data the newest generation trashed for it, unless something
exists at the path again, in which case the trashed copy stays. A package can
opt out with `keep_data = True`.

Attribution runs during every user-scope apply, even one with nothing to
change. The user state keeps the set of entries seen under the four XDG
directories; the first apply only records it, and later ones claim entries
that are new since and named like a package (its name, a Flatpak id or its last
segment, an npm scope or the name after it, case-insensitive). Claims live in
the user state under `data` and are dropped once the package or the path is
gone. Source-known data without a Flatpak id is a small built-in table of XDG
directory names keyed by package name; `data = [...]` covers the rest.

`fspec orphans` lists entries directly under those four directories that no
package claims (declared, source-known or attributed) and no managed file
sits inside. `fspec orphans --claim <path> <pkg>` records one as the package's
data. `fspec orphans --delete <path>` moves one to the user trash.

`fspec gc --older-than 14d` (units `s`, `m`, `h`, `d`, `w`) works per scope. A
generation is kept when it is the current one or its snapshot is newer than
the cutoff. Everything else last modified at or before the cutoff goes: its
`generations/<n>.json` snapshot, its trash directory, and, in the system
scope, store paths that no kept snapshot and nothing under `current` links to,
plus their `<dir>.json` sidecars and old `generations/<n>` link directories.
It prints each deletion and the space freed. `--older-than 0s` deletes
everything the current generation does not need.

## Files

Configs are generated from Starlark data and serialized to the target format
(JSON, TOML, YAML, INI, plist, Lua) or rendered from a template.

| Mode | On disk | When an app writes to it |
| --- | --- | --- |
| `managed` (default) | A real file, hash in state | `plan` shows the drift; `apply` overwrites unless `on_drift = "fail"`. `fspec adopt <path>` pulls the change back into the repo |
| `link` | Symlink into the repo working copy, stow style | The edit lands in the repo. If the app replaces the symlink with a file (an atomic rename), FormalSpec copies the content back and relinks |
| `seed` | Written once if missing | Never touched again |
| `patch` | A file the app owns; FormalSpec owns only the declared keys | Three-way merge, below |

Moving a `managed` file's `src` to another repo path with the same bytes, or
any `src` change of a `seed` file, is not a change: the plan shows nothing and
the next apply records the new `src` without rewriting the file.

Copies are the default, not symlinks: a copy survives apps that rename, chmod
or lock their config, and the hash in state detects drift just as well.

A `link` takes `src`, a file or a directory in the repo, or `target`, a path
outside it (absolute or `~/…`):

```python
file("~/.gradle", mode = "link", target = "/Volumes/Kyan Drive/gradle")
file("~/.claude-pulse/plugins", mode = "link", target = "~/.claude/plugins")
```

A `target` may be a directory and may not exist yet, like a folder on an
external drive that is not mounted; the link dangles until it is. A link whose
`target` is missing when the files phase commits is made after every other
phase, so `/etc/resolv.conf` onto resolved's stub does not cut DNS before the
packages that need it are installed. FormalSpec
owns only the link: when an app replaces it with a file, the link comes back
and nothing is copied into the target, and `fspec adopt` leaves it alone.

### Three-way merge for `patch`

```python
file("~/.claude/settings.json", mode = "patch", on_conflict = "ours", owns = {
    "model": "opus",
    "permissions.allow": list_union(["Bash(rg:*)", "Bash(fd:*)"]),
    "hooks.PostToolUse": list_keyed("matcher", hooks),
})
```

State keeps the value FormalSpec last wrote for each owned key, like
`kubectl apply`'s last-applied annotation. On apply, per key:

| On disk | Meaning | Result |
| --- | --- | --- |
| equals last-applied | untouched | write the new desired value |
| differs | the app or you changed it | `ours`: desired wins and plan shows it. `theirs`: keep it and stop managing the key until desired changes. `fail`: stop |
| missing | deleted | treated as a change |

Keys FormalSpec never declared are never read or written. Dropping a key from
`owns` deletes it only if it still holds the last-applied value.

A key with no last-applied value that already exists on disk is a conflict, except
for `list_union` and `list_keyed`, which join the existing list. Rewriting a patched file
reserializes it, so comments in it are lost. A conflict under `on_conflict = "fail"` aborts
`apply` during prepare, before anything live changes.

Lists need a strategy: `list_union` (ensure these items, keep others),
`list_replace` (exact list) or `list_keyed(field)` (merge objects by a field).

State records the sha256 of each managed file and secret, and `plan` also reports `source changed`
when the repo `src` differs from what was written. Removal deletes a file only while it still
matches that hash (a link: while it is still our symlink); otherwise it is kept with a warning.
`secret` decrypts with `<home>/.config/fspec/age.key`. After `fspec adopt` copies a file into the
repo it rewrites the state hash, so the file stops reading as drifted.

`fspec watch` uses inotify or FSEvents to report drift on managed files as it
happens and stays quiet about writes to keys a patch does not own.

## Services and init systems

`service(...)` describes the service, not the init system:

```python
service("syncthing", exec = "syncthing serve --no-browser",
        user = True, restart = "always", after = ["network"])
```

| Init | Rendered as |
| --- | --- |
| systemd | unit files, user units under `~/.config/systemd/user` |
| runit (Void) | `/etc/sv/fspec-<name>/run`, linked into `/etc/runit/runsvdir/default`; user services under a per-user `runsvdir ~/.local/service` |
| OpenRC | `/etc/init.d/fspec-<name>` with `supervise-daemon`; no user services |
| dinit | `/etc/dinit.d/fspec-<name>`, user services in `~/.config/dinit.d` |
| s6 | an s6-rc longrun source directory under `/etc/s6/sv`, recompiled on every change; no user services |
| launchd | plists in `/Library/LaunchDaemons` or `~/Library/LaunchAgents` |

Without `exec`, `service("gdm")` names a unit the distro ships and fspec only
enables it at boot, without starting it, so the same apply works inside the chroot
`switch --distro` builds. systemd runs `systemctl enable gdm`, runit links
`/etc/sv/gdm` into `/etc/runit/runsvdir/default` (and fails `apply` when the
package that ships it is missing), OpenRC runs `rc-update add gdm default` and dinit
`dinitctl enable gdm`. Such a service takes no other field, s6 and launchd refuse
it, and drift is the unit no longer being enabled. Removing it disables the unit
and leaves the distro's files alone. On systemd it may set `user = True` to enable
a user unit, and `display_manager = True` marks the login screen: the host then
boots to `graphical.target` (a distro can default to `multi-user.target`, which
never starts a display manager wanted by the graphical one), drift includes
another default, and removal puts back the default it replaced.

On systemd a running service whose `ExecStart` now resolves to another store
package than the one its main process was started from (`/proc/<pid>/exe`) is
stale: `plan` reports it as drift and `apply` restarts it. This covers a service
of either kind whose package moved in an earlier apply or in the other scope (a
user unit running a system store package). A package the same apply replaces is
caught after the commit, when the engine hands each backend the entries no
change touched (`refresh`). A unit with `RefuseManualStart` or `RefuseManualStop`
is left for its next start.

A timer's `schedule` is a five-field cron expression or one of `@hourly`,
`@daily`, `@weekly` and `@monthly`. Timers become systemd timers, cron entries or launchd
calendar intervals. Features only systemd has (socket activation, cgroup
limits, `graphical-session.target`) go under `systemd = {...}`, and launchd's
own keys under `launchd = {...}`; on another init `plan` fails and says which
field, rather than dropping it. A timer's `env` reaches its command on every
init (an `env K=V` prefix in a cron entry).

systemd units are `fspec-<name>.service` (and `.timer`) under
`/etc/systemd/system` or `~/.config/systemd/user`. An apply writes every new or
changed unit and reloads once before it enables and starts any, so a unit can
`Requires=` another from the same apply. A bare command name in `exec`
runs through `/usr/bin/env`, since systemd does not search `PATH`. `after =
["network"]` becomes `network-online.target`, another name `fspec-<name>.service`,
and a literal `*.target` stays as written. Under `systemd = {...}`, the keys
`unit`, `service` and `install` are dicts merged into those sections and any
other key is a `[Service]` directive; an extra replaces a generated directive of
the same name, and a list repeats the directive. A timer is a oneshot service plus
a timer with `Persistent=true`. A cron schedule that restricts both day-of-month
and day-of-week fails `plan` (cron ORs them, systemd ANDs them).

launchd jobs are `fspec.<name>` plists. `exec` is split with shell rules into
`ProgramArguments`; when it holds shell syntax (pipes, redirects, `$`, globs) it
runs as `/bin/sh -c`. `after` and `description` have no launchd equivalent and
are ignored. A timer becomes one `StartCalendarInterval` entry per combination of
listed values, capped at 64 entries. `launchd = {...}` holds raw plist keys
merged over the ones fspec writes, so `RunAtLoad = False, KeepAlive = False`
gives a job that is loaded but idle until `launchctl kickstart`, and
`StartInterval`, `WatchPaths`, `ThrottleInterval`, `StandardOutPath`,
`ProcessType` or `LimitLoadToSessionType` go in as written. Booleans, numbers,
strings, lists and dicts map to their plist types.

runit, OpenRC, dinit and s6 share two rules. `after = ["network"]` maps where the
init has a network service (OpenRC `need net`, dinit `depends-on = network` for
system services) and is ignored on runit and s6; a literal `*.target` fails
`plan`. Another service name orders after `fspec-<name>`. `description` is
written where the init has a place for it (OpenRC, a dinit comment) and dropped
elsewhere. `exec` is run as written by runit and s6 (`exec <exec>` at the end of
a `#!/bin/sh` script with `exec 2>&1` and `export K=V` lines before it). OpenRC
and dinit get an argument vector: shell syntax runs as `/bin/sh -c`, a bare
command name goes through `/usr/bin/env`, and dinit sets `env` through a leading
`/usr/bin/env K=V`.

runit services are `/etc/sv/fspec-<name>/run`, linked into the default runlevel
`/etc/runit/runsvdir/default`, which `/var/service` resolves to once booted. The
link cannot go through `/var/service` itself: that points into `/run`, which a
chroot (`switch --distro`) does not have.
runsvdir starts a new link within five seconds, so an add runs no `sv` command;
an update runs `sv restart` and a removal `sv down` before unlinking. runit
always restarts, so `restart = "no"` adds a `finish` running `sv down .` and
`on_failure` one that does so only on exit status 0. A dependency becomes an
`sv check ../fspec-<dep> || exit 1` line in `run`. User services are plain
directories in `~/.local/service`, scanned by `runsvdir` running as the system
service `fspec-runsvdir-<user>` (`/etc/sv/fspec-runsvdir-<user>/run`, started
with `chpst -u <user>`). The first user service creates it, so that first apply
needs the privileges to write `/etc/sv` and `/etc/runit`; the runsvdir service
is not part of the user state and stays when the last user service goes.

OpenRC services are `/etc/init.d/fspec-<name>` (`#!/sbin/openrc-run`) with
`supervisor=supervise-daemon`, `command`, `command_args` and a `depend()` block,
added with `rc-update add fspec-<name> default` and started with `rc-service`.
supervise-daemon respawns on any exit, so `on_failure` behaves as `always`;
`restart = "no"` drops the supervisor and uses `command_background`. OpenRC has
no user services, so `user = True` fails `plan`.

dinit services are `/etc/dinit.d/fspec-<name>` (or `~/.config/dinit.d/`) with
`type = process`, `command`, `restart = true | on-failure | false` and
`depends-on`. Apply runs `dinitctl [--user] enable` and `start`; an update runs
`reload` then `restart`, since dinit keeps the description it loaded. `enable`
needs the `boot` service to have a `waits-for.d` directory.

s6 services are s6-rc longrun source directories, `/etc/s6/sv/fspec-<name>/`
with `type`, `run`, an optional `finish` (exit 125 stops s6-supervise from
restarting) and `dependencies.d/fspec-<dep>`. Every fspec service is also listed
in the bundle `/etc/s6/sv/fspec`, which the host's boot bundle must include.
After the files change, fspec compiles all of `/etc/s6/sv` into
`/etc/s6/db/fspec-g<generation>-<a|r>` with `s6-rc-compile`, switches to it with
`s6-rc-update`, and starts the bundle with `s6-rc -u change fspec`. Old
databases stay in `/etc/s6/db`. fspec sets up no per-user s6 tree, so
`user = True` fails `plan`.

Timers on these four inits are cron entries with the schedule written as given.
A system timer is `/etc/cron.d/fspec-<name>` (run as root); a user timer is a
block between `# BEGIN fspec:<name>` and `# END fspec:<name>` in the user's
crontab, read with `crontab -l` and written with `crontab -`. A timer name with a
`.` fails `plan` (run-parts crons skip such files), and `%` in `exec` is
escaped. fspec assumes a cron that reads `/etc/cron.d` (cronie, vixie-cron) and
does not detect the flavour; dcron, busybox crond and snooze are out of scope.

`user` creates Linux accounts with `useradd` and adds missing groups with
`usermod -aG` (it never removes a group). The account exists from the users
phase, so files and secrets can be owned by it, but it joins its groups only
after every phase has committed: packages create groups such as `docker` (a
sysusers.d file) and `nordvpn` (a postinstall that also empties the group), so
one apply converges. A group still missing then gets one `systemd-sysusers`
run on a systemd host, since a sysusers.d file a package or a `file` dropped
is otherwise read only at the next boot. On macOS it only sets the login shell
and checks the account and uid; it never creates accounts. Removing a `user`
stops managing it and leaves the account. `defaults` journals the previous value
of each key so a failed apply restores it, and removal deletes a key only while
it still holds the value fspec wrote. `defaults(..., system = True)` writes the
machine-wide domain in /Library/Preferences as root, in the system scope.

`setting(tool, key, value)` covers the macOS settings that have a command and
no defaults domain: `setting("pmset", "sleep", 0)` runs `pmset -a sleep 0` and
checks every power source in `pmset -g custom`, `setting("systemsetup",
"restartfreeze", True)` runs `systemsetup -setrestartfreeze on`, and
`setting("firewall", "globalstate", False)` runs `socketfilterfw
--setglobalstate off`. pmset takes a number, the others a bool. A failed apply
restores the previous value (per power source for pmset). Removing a `setting`
stops managing it and leaves the value, because turning one back has no safe
default: remote login off cuts ssh. `systemsetup -setremotelogin` needs Full
Disk Access for the terminal fspec runs in.

## System layer

The system layer is the kernel, bootloader, drivers and base packages.

On bootc distros (Fedora Atomic, Bazzite, Aurora, Bluefin, CentOS, and
community images for others) it is an image:

```python
system = image(
    base = "ghcr.io/ublue-os/bazzite-nvidia:stable",
    packages = dnf(["asusctl", "tuned-ppd"]),
    kernel_args = ["lockdown=integrity", "iommu=force"],
)
```

`fspec image containerfile --host g815` prints the Containerfile: `FROM` the
base, `dnf install -y` for the `dnf(...)` packages (other package managers are
an error), the kernel arguments as `/usr/lib/bootc/kargs.d/fspec.toml`
(`kargs = [...]`), and `bootc container lint` last.

`fspec ci init` writes `.github/workflows/fspec-images.yml` next to the spec.
The workflow `fspec-images` runs on pushes to main, weekly and on demand. For
each host with an `image(...)` it renders the Containerfile, builds it with
podman and pushes `ghcr.io/<owner>/fspec-<host>:latest`. The owner comes from
the `origin` remote, or `--registry ghcr.io/<owner>`. A second job runs
`fspec update --images --registry …`, which resolves each tag to its digest
(anonymous ghcr token, then the manifest request; the packages must be public)
and opens a PR with the new `[system.<host>] image = "<repo>@sha256:…"`. Rerun
`fspec ci init` when hosts are added.

Apply runs `bootc switch <repo>@<digest>` when the locked digest differs from
the staged or booted one in `bootc status --json`, journaling `bootc rollback`
as the undo, and prints that a reboot activates it. With no lock entry, `plan`
says to run `fspec ci init`, let the workflow push, then `fspec update
--images`. Rollback is the previous deployment in the boot menu.

On classic distros (Fedora Workstation, Arch, Debian, Void) it is
`fedora(...)`, `arch(...)` and so on, driving the package manager directly with
cached pinned packages, and kernel rollback comes from btrfs snapshots.

The `packages` list of `fedora(...)`, `arch(...)`, `debian(...)` and `void(...)`
is installed by name with `dnf`, `pacman -S --needed`, `apt-get` and
`xbps-install`, and a name removed from the list is removed from the machine.
The base packages are not in the lock, so they are never pinned or cached;
pin a package that matters by declaring it as a `pkg` as well. Dropping the
whole `fedora(...)` declaration leaves its packages installed. On zypper
distros a `!name` entry, zypper's own syntax, removes that package in the same
transaction, for one that conflicts with a wanted one
(`"tuned-ppd", "!power-profiles-daemon"`); drift reports it when it comes
back. A failed apply keeps the packages it installed and writes no state, so
the next apply plans the same installs and finds them done; it reverses
removals, upgrades and repo changes, not installs. Removing what a first apply
installed would take thousands of packages and as long again as installing
them, and putting back a `!name` the wanted set conflicts with fails anyway.
Flatpak and Homebrew installs work the same way. Before any
change to the system layer, if `findmnt -no FSTYPE /` says btrfs, apply takes a
read-only snapshot `/.snapshots/fspec-<generation>`; it is taken while
preparing, before the package phases commit, so it holds the old kernel. A
failed apply leaves the snapshot behind. Other filesystems get no snapshot.
`pkg` sources for the four managers cache the locked `.rpm`, `.pkg.tar.zst`,
`.deb` or `.xbps` under `/var/cache/fspec/pkgs` and install the file (`dnf
install`, `pacman -U`, `apt-get install`, and for `.xbps` a local repository
directory built with `xbps-rindex -a` and used through `xbps-install -R`). A
rollback reinstalls the previous cached file, or the plain name when it is gone.
Xbps has no exact-version flag here, so the local repository pins by being the
only place that file lives; if the Void repos hold a newer version, xbps may
prefer it.

These packages commit before services, so a `service("sshd")` can enable a unit
the same apply installed: the `packages` list in the distro phase, `pkg`
sources in the package phase after it.

Installed versions, for the record after an install and for drift in `plan`,
come from one query per thousand names (`rpm -q --qf`, `pacman -Q`,
`dpkg-query -W`), so a 3000-package base costs a few processes, not 3000.
A name the batch does not report, such as a capability or `name:arch`, is
queried on its own. xbps and moss have no such query and ask per name.

New `pkg` packages installed by name commit in one transaction per manager,
so thirty zypper packages cost one solver run instead of thirty. When that
transaction fails, they are installed one at a time: the ones that can land
do, and the error names the package that cannot.

AerynOS's `aerynos(packages = [...])` and `pkg(..., source = moss())` run
`moss install` and `moss remove`. moss builds every transaction as a new
`/usr` from its deduplicated store and swaps it in atomically, recording a
numbered state (`/usr/.stateID`). fspec maps its rollback onto those states:
before a moss removal or upgrade it journals `moss state activate <the current
state>`, so a failed apply returns the system to the state it started from
rather than reversing package by package. An install is kept, as on every
other manager. The installed
version comes from `moss list installed`. Every fspec command run on AerynOS
sticks to plain sh, since its base system has no grep, sed or awk.

openSUSE Tumbleweed's `tumbleweed(packages = [...])` and `pkg(..., source =
zypper())` run `zypper --non-interactive install --auto-agree-with-licenses`
and `zypper --non-interactive remove`; versions come from `rpm -q`. A
Tumbleweed host needs a `[zypper.<host>]` pin in the lock, and `plan` asks for
`fspec update` without one. Apply points the distro repos at that snapshot:
it writes the snapshot date to the zypp repo variable
`/etc/zypp/vars.d/fspec_snapshot`, writes `/etc/zypp/repos.d/fspec-oss.repo`
and `fspec-non-oss.repo` with
`baseurl=https://download.opensuse.org/history/${fspec_snapshot}/tumbleweed/repo/oss/`
(and `non-oss`), disables the rolling repos on download.opensuse.org and
cdn.opensuse.org (`repo-oss`, `repo-non-oss`, `repo-update`, and the debug and
source ones when enabled) with `zypper modifyrepo --disable`, force-refreshes
the snapshot repos and runs `zypper --non-interactive dist-upgrade
--auto-agree-with-licenses`. Moving the pin rewrites only the variable and runs
the upgrade again, which also downgrades when the pin moved back. Other repos
such as openh264 are left alone. The undo puts the old variable and repo files
back, re-enables the rolling repos and runs the same upgrade against them, so
a failed apply returns to the snapshot it started from. `plan` reports drift
when the variable, the snapshot repos or an enabled rolling repo disagree with
the lock.

Third-party repos are `repo(...)` resources, for zypper distros only:

```python
repo("1password", url = "https://downloads.1password.com/linux/rpm/stable/x86_64",
     key = "https://downloads.1password.com/linux/keys/1password.asc",
     fingerprint = "3FEF9748469ADBE15DA7CA80AC2D62742012EA22")
```

`key` and `fingerprint` go together. Prepare downloads the key to
`/var/cache/fspec/keys/<name>.asc` and checks it with `gpg --show-keys`; a
different fingerprint stops the apply before anything changes. Commit imports
it with `rpm --import` unless `gpg-pubkey-<last 8 hex digits>` is already
installed (libzypp trusts keys in the rpm database), then runs `zypper addrepo
--refresh <url> <name>`, replacing a repo of the same alias at another URL.
Removing the resource runs `zypper removerepo` and keeps the key. Repos and the
snapshot commit in the distro phase, before `pkg` packages, so a package from
the repo installs in the same apply. The base packages commit in that phase
too, after the repos.

A repo the host cannot reach is deferred, not an error: its key cannot be
fetched, or `zypper refresh <name>` fails after the add (the vendor is down,
or a filtering proxy such as Cisco Umbrella intercepts its TLS). The apply
prints a warning, leaves the repo disabled so the distro upgrade still works,
and records nothing for it, so the next apply tries again. A package that says
where it comes from, `pkg("nordvpn", source = zypper(repo = "nordvpn"))`, is
deferred with its repo, and so are the services, timers and files it owns that
commit in later phases. A user's group that does not exist yet waits the same
way. A fingerprint mismatch still stops the apply.

macOS has no equivalent of either; the system layer there is `defaults`,
`setting`, launchd and Homebrew.

### Secure Boot and signed kernels

Stick with distros whose kernels carry well-known signing keys when a game's
anti-cheat requires it. Epic's EAC Linux v2 trust list (October 2026) includes
Fedora, Universal Blue, Canonical, Debian, openSUSE and Rocky, but not Arch,
CachyOS or NixOS. `image(...)` sets the required kernel arguments; firmware
settings (Secure Boot, TPM) stay manual.

## Switching distros

Personal files are never managed. FormalSpec only touches paths it declared or
attributed to a package, and `protect = ["/home", "/var/home"]` makes it refuse
any action under those paths. The list is exactly what the host declares, with
no default. Plan fails on a file, secret or data path under an entry. An entry
that is an ancestor of the applying user's home leaves that home usable, since
the spec has to manage it; other users' homes stay refused. The spec pins your UID so ownership survives a
hop.

Between bootc distros, change `image(base = ...)` and apply. FormalSpec
rebuilds your image on the new base and runs `bootc switch`; you reboot into
the new distro with the old one still in the boot menu. Home, Flatpaks, store
packages and user services are outside the image and carry over.

`fspec switch --reset-var` (run after that apply, as root) resets `/etc` and
`/var` to the new image's defaults. The running system cannot reset its own
`/etc`, so the command only stages the reset for the next boot. It refuses
unless the machine is a bootc deployment with a staged image. It writes, under
`/var/lib/fspec/reset-var/`, a `keep` list (fspec's own state `/var/lib/fspec`,
`/opt/fspec` and `/var/opt/fspec`, the homes `/home`, `/var/home` and
`/var/roothome`, then the host's `persist` paths), a `reset.sh`, and finally a
`pending` marker. It also installs and enables `fspec-reset-var.service`, a
oneshot ordered after `local-fs.target` and before `sysinit.target` and
`systemd-tmpfiles-setup.service`, which only runs while `pending` exists. The
script deletes everything under `/var` and `/etc` that is neither in `keep` nor
a parent of something in it (it descends into parents instead of deleting
them), copies `/usr/etc` over `/etc`, and removes `pending` last, so an
interrupted run repeats on the next boot. tmpfiles then recreates the image's
`/var` defaults. Remove `pending` to cancel. The account databases
(`/etc/passwd`, `/etc/shadow`, `/etc/group`, `/etc/gshadow`, `/etc/subuid`,
`/etc/subgid`) are always kept, like the homes: `apply` recreates declared users
but not their passwords.

To or from a classic distro, `fspec switch --distro void` (fedora, arch,
debian, ubuntu, void or aerynos). The host's `system` must already declare the target
distro, so edit `system =` to `void(...)` first and pass `--host` for that
host; the command refuses any other host. Nothing is built from a second
declaration. Every root lives on one disk in one of the layouts below, side by
side with the others, and boots through one shared bootloader.

### Disk layout

The default layout, the one `fspec disk init` creates and the one to use:

- A GPT disk.
- A 2 GiB EFI system partition (FAT32), mounted at `/efi` in every root as the
  UAPI Boot Loader Specification places it, and shared by every distro and by
  Windows if it is there. 2 GiB holds several distros' unified kernel images
  next to Windows' loader.
- Optionally a LUKS2 container holding the rest of the disk.
- One btrfs filesystem with the subvolumes `@<distro>` (one per root), `@home`
  (shared), `@swap` (no copy-on-write, holding `swapfile`), `@snapshots`, and
  `@fspec` mounted at `/opt/fspec`, so a switch does not download the store
  again. Store paths are keyed by the artifact's hash and the lock keys
  artifacts by platform, so every root of a host converges on the same store
  and `current`.
- systemd-boot on the ESP. Kernels go onto the ESP per distro, so roots never
  overwrite each other's kernels or bootloader config: unified kernel images
  as `/efi/EFI/Linux/<distro>-*.efi` where the distro builds them, otherwise
  `/efi/<distro>/` with a Boot Loader Specification entry in
  `/efi/loader/entries/<distro>-*.conf`. systemd-boot finds both at boot time,
  so a kernel update in a root that does not own boot never leaves a stale
  entry. Each root keeps its own `/boot` for the distro's packages, and its
  own tooling writes the ESP files: `kernel-install` with `layout=uki` on
  Fedora, Debian and Ubuntu, a `default_uki` mkinitcpio preset on Arch, and a
  kernel hook fspec installs on Void, which has no UKI tooling. The kernel
  command line comes from each root's `/etc/kernel/cmdline`.
- With LUKS, every root gets an `/etc/crypttab` line, `cryptsetup` in its
  initramfs and `rd.luks.name=` on its command line.

Beside Windows the layout keeps Windows' ESP, which Windows' setup makes 100
to 300 MB, too small for several unified kernel images, and cannot grow in
place since `C:` or its reserved partition follows it. A 2 GiB XBOOTLDR
partition (type `ea00`, FAT32) holds the kernels and entries instead and is
mounted at `/efi`, so every recipe's kernel tooling writes where it always
does. Windows' ESP is mounted at `/boot/efi` and holds systemd-boot and
`loader.conf`, which systemd-boot reads only from its own ESP; it reads
entries and UKIs from both partitions, and adds Windows Boot Manager from
`EFI/Microsoft/Boot/bootmgfw.efi` on its ESP. fspec never writes under
`EFI/Microsoft`. `bootctl install --esp-path --boot-path` puts its firmware
entry first in `BootOrder` and keeps Windows' entry, and `loader.conf` gets
`timeout 5` so the menu shows. `/etc/kernel/install.conf` sets
`BOOT_ROOT=/efi`, since kernel-install's own search would find Windows' ESP
at `/boot/efi` before the XBOOTLDR partition, which it looks for only at
`/boot`. AerynOS is refused here: blsforme finds the ESP itself and puts its
kernels on it. Two ESPs on one disk would avoid the second partition type,
but systemd-boot cannot chainload Windows from another partition, and
Windows' updates expect its own ESP.

Two more layouts are supported:

- ZFS: a pool `rpool` with `rpool/ROOT/<distro>` boot environments,
  `rpool/home` and `rpool/fspec`, booted by ZFSBootMenu from the ESP.
  ZFSBootMenu is built for several boot environments on one pool: it reads
  each root's kernels from that root's own `/boot`, so nothing is copied to
  the ESP. Every root needs the ZFS module in its initramfs, which Debian
  (from `contrib`, built by DKMS), Ubuntu and Void provide; Arch and Fedora
  ship ZFS only from third-party repositories, so fspec refuses them on ZFS.
  Out-of-tree ZFS modules are not signed by the distro's keys, Ubuntu's being
  the exception, so Secure Boot, and with it EAC Linux v2, only works with
  Ubuntu on ZFS; `plan` says so.
- BIOS (or an MBR disk): GRUB in BIOS mode, from a `bios_grub` partition on
  GPT or the MBR gap, and a shared ext4 boot partition mounted at
  `/var/lib/fspec/boot` in every root. Each root bind-mounts its own folder
  `<distro>/` from it at `/boot`, so its packages write their kernels there,
  and GRUB itself lives in the folder of the root that owns boot. No unified
  kernel images. GRUB's menu is written when the owner runs `grub-mkconfig`,
  so every root keeps stable kernel names in its folder (Arch's own
  `vmlinuz-linux`, Debian's `link_in_boot` symlinks, and a kernel hook on
  Fedora and Void). `plan` says that Secure Boot, and so EAC, is impossible
  there.

Each distro's recipe declares what it needs: the smallest ESP it fits on,
the filesystems it boots from, whether it builds unified kernel images, and
whether bootc can install it (`--to-image` needs EFI and a btrfs root, since
bootc supports neither a ZFS root nor the shared BIOS boot partition). `fspec
plan` prints the layout it found and, when the host declares a distro other
than the running one, whether the switch fits the disk; a switch the disk
cannot satisfy is refused with the fix, usually a `fspec disk` command.

### Getting a disk onto a supported layout

- `fspec disk plan [<device>]` prints the steps that bring the running disk
  (or `<device>`) onto a supported layout, and which of them need the disk
  unmounted.
- `fspec disk migrate [<device>]` runs the steps that can be done in place:
  `btrfs-convert` from ext4, adding the shared subvolumes and moving `/home`
  and `/opt/fspec` into them, moving a root that sits at the top level into
  `@<distro>`, putting the root's kernels on the ESP, and carving a 2 GiB ESP
  (or the BIOS boot partition) out of the btrfs partition: the filesystem and
  its partition shrink, a new partition takes the freed space at the end of
  the disk, the old ESP's files are copied over, and the new ESP takes the
  old one's partition GUID and FAT serial, so firmware entries and every
  root's `fstab` keep pointing at it. The old ESP is retyped as Linux data
  and given a new GUID and serial, and stays as a copy. A partition cannot be
  grown into the space in front of a btrfs filesystem, which is why the ESP
  moves rather than grows. Encrypting an existing disk is not migrated, and
  neither is shrinking a btrfs filesystem inside LUKS; both need
  `fspec disk init`. An ext4 root converts only while unmounted, so
  `disk migrate <device>` runs from a live system, keeps the filesystem
  UUID, fixes the root's `fstab` line and rebuilds its initramfs and GRUB
  config; the rest of the steps run after booting it. After a reboot into
  `@<distro>`, `disk plan` lists what the move left behind (the old root's
  files, the copies of `/home` and `/opt/fspec` under their new mounts) as
  destructive steps.
- A NixOS root is different: its `fstab` and initrd come from its
  configuration, and every generation in its boot menu mounts `/` as ext4.
  `disk migrate <device> --nixos-system <path>` takes a system built to mount
  `/` as btrfs `subvol=@nixos` on the same UUID (and `/home` from `@home`,
  `/swap` from `@swap`), already copied into the root's `/nix/store`, and
  refuses without one or when its `fstab` does not say so. It refuses
  without an `fspec-live` entry on the ESP, or while the EFI variable
  `LoaderEntryDefault` is set, and first makes `fspec-live*` the
  `loader.conf` default, so a reset during the migration comes back into the
  live system. It converts with
  `btrfs-convert --uuid copy`, keeping `ext2_saved`, snapshots the top level
  into `@nixos`, creates `@home` (a reflink copy of `/home`), `@swap` with
  `chattr +C` (NixOS creates the swapfile), `@snapshots` and `@fspec` (with a
  reflink copy of `/var/lib/fspec-live`), and leaves the default subvolume at
  the top level. It then makes the system a new generation of the system
  profile and runs its `switch-to-configuration boot` in a chroot of `@nixos`
  with the ESP, `/dev`, `/proc`, `/sys` and efivarfs mounted, so NixOS writes
  its own entry. The older generations cannot boot btrfs, so their entries
  move to `loader/entries.ext4/` on the ESP; the new one gets three boot
  tries, `boot.panic_on_fail panic=10` (a failed initrd reboots and burns a
  try) and `systemd.mask=` for `emergency` and `rescue` (a failed mount after
  the initrd still reaches `sshd`), `loader.conf` loses its `default`,
  the profile drops the ext4 generations, so the next `nixos-rebuild` does
  not write their entries again (`ext2_saved` keeps them for the way back),
  and the `fspec-live` entry's `findiso=` and `fspec.overlay=` move to
  `/@fspec/live/`. After three failed boots systemd-boot falls through to
  `fspec-live`; NixOS's `systemd-bless-boot` marks a good boot. Once it has,
  a `systemd.run=` on the entry (started by `systemd.wants=`, as below) drops those arguments again, so later boots
  keep their emergency shells; `fspec disk unmask` does the same by hand.
  Every step
  journals its undo, synced before the step runs, in `fspec/disk-journal` on
  the ESP: a failure, or the next run after a kill or a reset, runs
  `btrfs-convert -r` and puts the ESP's `loader`, `EFI/systemd` and
  `EFI/BOOT` back, so NixOS boots from ext4 as before. After a successful run, `fspec-compress.service` recompresses
  `@home` and `@nixos` (not the store) with zstd until the filesystem is 80%
  full or the live system reboots. From the booted `@nixos`, `disk plan` lists
  deleting the old top level with `ext2_saved` (which ends the way back to
  ext4), the copy of `/home` in `@nixos` and its old `/swapfile`, and refuses
  the first while `fspec-live` still finds its files outside `@fspec`.
- `disk migrate --stage <iso> --nixos-system <path>`, run on the NixOS
  root itself, sets that up for one unattended run on the next boot. It
  checks the system as above against the running root, refuses a dynamically
  linked fspec, asks for the root's device name and a backup (`--backup` or
  `--no-backup`), and then copies the ISO and an overlay to
  `/var/lib/fspec-live` on the root, the ISO's kernel and initrd to
  `fspec-live/` on the ESP, writes `loader/entries/fspec-live+3.conf` and
  makes it the `bootctl set-oneshot` entry. A GC root keeps the system in the
  store until the migration has made it a generation. The overlay is
  `--overlay <tar>` (network, ssh keys, a `guard.conf` of its own) with
  fspec's files appended: the running fspec at `/usr/local/bin/fspec`, so the
  live system runs the engine that staged it, `/etc/fspec-live/migrate`, and
  a `guard.conf` (ten minutes to come online, the hardware watchdog at 30
  seconds, the ESP to bless on) unless the host's overlay has one. The entry
  boots `toram`, so the root is unmounted once the ISO is in RAM, and carries
  `fspec.migrate`, `systemd.run=` for the script and
  `systemd.wants=kernel-command-line.service`: `systemd.run=` alone only
  swaps the default target, which the live system's own `default.target`
  outranks. The script waits for the guard, types the device name into
  `disk migrate <device> --nixos-system <path> --no-backup`, writes its log
  to `fspec/migrate.log` on the ESP and reboots, into the new NixOS
  generation or, after a rollback, into NixOS on ext4; it stays up for ssh
  only when an unfinished journal is left. A live boot that never comes
  online reboots on its own and costs one of the entry's three tries, and the
  one-shot variable means the boot after it is NixOS again. The migration
  drops `fspec.migrate` and the `systemd.run=` arguments from the entry
  along with moving its paths to `@fspec/live`, so later boots of it are a
  plain live system; after a reset mid-migration the script still runs,
  because it also starts when the ESP holds a journal.
- `fspec disk init <device> [--layout default|zfs|bios] [--luks] [--distro d
  --host h]` partitions and formats a disk from scratch, creates the shared
  subvolumes or datasets, installs the bootloader, and with `--distro`
  installs the first root the same way `switch --distro` does. It refuses a
  partition, a disk with anything mounted, and a disk under 16 GiB. ZFS
  takes no `--luks`, since it encrypts datasets itself.
- Beside Windows (`fspec_switch::init` with `windows`, which only the installer
  sets so far), `init` leaves the partition table and Windows' partitions in
  place and creates the XBOOTLDR and btrfs partitions in one space: the
  largest free space when it holds 32 GB, or space shrunk off the end of
  `C:`, the largest basic data partition holding NTFS. The shrink runs
  `ntfsresize --no-action` as a probe, then `ntfsresize --size`, then
  `sfdisk -N` with only the size, so the partition keeps its start, GUID,
  name and boot sector. Linux takes 32 GB at least, and Windows keeps 20 GiB
  more than `ntfsresize --info` says it needs. `ntfsresize` refuses a
  hibernated volume (Fast Startup) and one marked for `chkdsk`; a BitLocker
  volume cannot be resized from Linux at all, so it has to be shrunk in
  Windows' Disk Management, leaving free space. Once the partitions exist, an
  undo step deletes them if a later step fails. `C:` stays shrunk, and
  `ntfsresize` has marked it for a check, so the next try finds the space free
  instead of shrinking `C:` again; the installer's "Try again" reads the disk
  anew for that.

Every destructive step asks for the device name to be typed back, refuses a
mounted target unless the step is built for one (`btrfs filesystem resize`
and the subvolume moves run on the mounted filesystem), and first checks that
a backup exists: a read-only snapshot under `@snapshots` taken in the last 24
hours, or `--backup <path>` naming a backup modified in the last 24 hours. A
blank disk needs neither.

### Switching

The switch reads the machine through the runner: the layout (`lsblk` and
`findmnt`), the firmware (EFI when `/sys/firmware/efi` exists), the running
root and its kernel command line. Then it:

1. Checks the target distro's recipe against the layout and stops with the
   fix if it does not fit. It refuses unless the running root owns boot when
   the layout is BIOS (see below). The first switch on a machine records the
   running root as the owner.
2. Creates the root, `@<distro>` or `rpool/ROOT/<distro>`, and mounts it under
   `/run/fspec/switch/`.
3. Bootstraps the base system from the running OS: `dnf --installroot` (needs
   `--release <n>`; the Fedora repositories are named on the command line,
   since a fresh root has none), `pacstrap -K`, `debootstrap` (`--release` is
   the suite, default `stable` on Debian and `noble` on Ubuntu) or
   `xbps-install -r` (answering its repository key prompt). dnf downloads ten
   packages at a time (`max_parallel_downloads=10`, also on every apply's
   `dnf install`). The bootstrap includes the recipe's boot packages for the
   layout: UKI tooling, GRUB, `cryptsetup` or ZFS, and git on every distro, since
   fspec checks out `git(...)` sources with it, for itself and as a builder.
   Debian and Ubuntu get theirs from apt in the new
   root right before the apply, along with the `-updates` and `-security`
   pockets and a `full-upgrade`, so the apply installs current versions once
   and records the versions that stay installed.
4. Writes the new root's `fstab` (`/`, `/home`, `/efi`, `/opt/fspec`, the
   swapfile, and on BIOS the shared boot partition and the `/boot` bind),
   `crypttab`, `/etc/kernel/cmdline` and the recipe's kernel tooling config,
   and copies `resolv.conf`. The command line keeps the running kernel's
   options (`console=`, `net.ifnames=0`, extra `rootflags=`) with only the
   root swapped, since the machine's serial and network setup can depend on
   them.
5. Applies the spec inside it. `--root` only logs commands, so the apply runs as
   `chroot <new root> fspec --spec <copy of the spec> apply --local --scope
   system --host <host>` with the running `fspec` and the spec directory copied
   in, the shared `/home`, `/opt/fspec` and `/efi` bound in (so a declared
   user's home lands in the shared one), and a `PATH` with the `sbin`
   directories, which Arch's lacks. The new distro's backends do the work. The
   binary must run in the new root, so it has to be static: a glibc or
   nix-store build does not run in a fresh root. systemd units are enabled
   without `--now`, which `systemctl` refuses in a chroot, and then started,
   which it ignores there. The user scope applies at first login.
6. Runs the recipe's kernel tooling in the new root once, which puts its
   kernels where the layout boots them (on ZFS it sets the boot environment's
   `org.zfsbootmenu:commandline`), and on BIOS regenerates the owner's GRUB
   menu.
7. Unmounts, also after a failure, and says to run `fspec switch --adopt-boot`
   from the new root before forgetting the old one.

Under `--root` the probes are skipped and the default layout is assumed, with
the rooted system owning boot, so the plan is logged to
`<root>/var/log/fspec-commands.log` without running anything.

### Who owns boot

Exactly one root owns boot: on the default layout the one systemd-boot boots
by default, on ZFS the pool's `bootfs`, on BIOS the one whose GRUB the boot
sector loads. On BIOS the owner is recorded with its kernel command line in
`.fspec-boot-owner` on the btrfs top level, which every root can mount. `fspec
plan`, run as root on a classic distro, prints which root owns boot.

`fspec switch --adopt-boot`, run from the new root, makes it the owner:

- Default layout: `default <distro>-*` in `/efi/loader/loader.conf` (beside
  Windows, `/boot/efi/loader/loader.conf`), and `bootctl set-default ""` so no
  EFI variable outranks it.
- ZFS: `zpool set bootfs=rpool/ROOT/<distro> rpool`.
- BIOS: `grub-install --target=i386-pc --boot-directory=/boot` (`grub2-install`
  on Fedora) to the disk's boot sector, and `grub-mkconfig`, whose
  `/etc/grub.d/42_fspec` lists every other root it finds in the shared boot
  partition with the kernel options that root recorded there. Then the marker.

bootc roots keep bootupd as their bootloader owner; `--adopt-boot` refuses them
for now and says so.

### AerynOS

AerynOS's kernels and boot entries are managed by blsforme, which moss runs
after every transaction. It fits the default layout without fspec's own
kernel tooling: it writes the kernel and its prebuilt initramfs to
`/efi/EFI/aerynos/<version>/` and Boot Loader Specification entries
`/efi/loader/entries/aerynos-<version>-<state>.conf` for the newest five moss
states (each with `moss.fstx=<state>`, so the menu doubles as moss's rollback),
and garbage-collects only `aerynos` and `serpentos` entries and folders. Three
things need handling, since AerynOS assumes it is alone on the ESP:

- blsforme rewrites `loader.conf` to `default "aerynos*"` on every
  transaction, which would make AerynOS the boot owner after any package
  change. fspec runs moss through a wrapper that puts `loader.conf` back unless
  AerynOS already owned boot, and reads `default "aerynos*"` as AerynOS owning
  it. A `moss` run by hand still takes boot; `fspec switch --adopt-boot` from
  the owner gives it back.
- blsforme also copies AerynOS's systemd-boot over `EFI/BOOT/BOOTX64.EFI` and
  `EFI/systemd/systemd-bootx64.efi` whenever they differ, so the shared
  bootloader is whichever version moss last installed.
- blsforme puts `rootfsflags=subvol=` on the command line, which neither dracut
  nor systemd reads, so the root's subvolume comes from fspec's
  `/etc/kernel/cmdline.d/fspec.cmdline`, written before moss first syncs boot.

`fspec switch --distro aerynos` bootstraps with the host's `moss -D <root>`
(the static build from AerynOS's os-tools releases has to be on `PATH`), from
the `unstable` stream with `pkgset-aeryn-base`, `linux-stable`,
`linux-firmware` and `btrfs-progs`, with the ESP mounted in the new root first
because moss syncs boot as part of the install. Inside the new root, blsforme
would take the ESP the running bootloader names in its EFI variables, which
from an installer is the installer's own, so the chroot gets an empty
`efivars` and blsforme falls back to the ESP on the root's disk. Each kernel
ships a 200 MB generic initramfs, so AerynOS needs a 1 GiB ESP and about 450
MiB free, and it boots only through systemd-boot: the ZFS and BIOS layouts
refuse it. `fspec forget aerynos` also deletes `/efi/EFI/aerynos`. AerynOS
links `/usr/local` to `/var/local` and its non-interactive ssh `PATH` leaves out
`/usr/local/bin`, so the remote commands add it themselves.

`fspec switch --to-image` turns a classic machine into the host's bootc image
with `podman run ... <locked image> bootc install to-existing-root
--acknowledge-destructive`. It needs an `image(...)` host with a digest in
`fspec.lock`, refuses a machine that already runs bootc, refuses a layout
bootc cannot install to, and needs `--yes` because the install reinitialises
`/boot`.

`fspec switch --to-image --beside <name>` installs the image into a new
subvolume `@<name>` on the running root's btrfs filesystem instead, and
leaves the running root, `loader.conf`, the other roots' entries and the EFI
variables alone. It needs EFI, a btrfs root and systemd-boot on an ESP
mounted at `/efi`, `/boot/efi` or `/boot`, so it also works from a root that
is not on the default layout yet. It refuses an existing `@<name>`, and a
user the spec declares that the inherited root's `/etc/passwd` lacks, before
it creates anything. When a later step fails, it deletes the `@<name>` it
created (`btrfs subvolume delete --recursive`, with the top level mounted
again after the cleanup), so the next try starts clean. It runs
`podman run ... -v <new root>:/target:rbind <locked image> bootc install
to-filesystem --bootloader=none --skip-finalize --root-mount-spec=UUID=<fs>
--karg=rootflags=subvol=@<name>,noatime,compress=zstd:1 --karg=panic=10
--karg=rd.shell=0 --karg=rd.emergency=reboot /target`, plus the running
`console=` options. `--skip-finalize` matters: finalizing remounts the
target read-only and freezes it, which on a shared filesystem reaches the
running root. The kernel options make a boot that fails in the initramfs
reboot instead of waiting at a shell.

ostree keeps its kernels in the subvolume, where systemd-boot cannot read
them. `/etc/fspec/esp-sync.sh` copies each deployment's kernel and initramfs
to `<esp>/<name>/<ostree boot checksum>/` and writes
`<esp>/loader/entries/<name>-ostree-<n>+3.conf` with ostree's title and
options and three boot tries. `<n>` is given once per deployment and recorded
in `<esp>/<name>/ids`, so the newest deployment sorts first and an entry keeps
its counter when it is rewritten. The entries have no `sort-key`, so the
other roots' entries that have one (NixOS writes `sort-key nixos`) sort before
them, and systemd-boot sorts an entry with no tries left after every other
entry. The script only touches `<esp>/<name>/` and
`<name>-ostree-*.conf`, and deletes the entries and kernels of deployments
ostree dropped. The install runs it once, and in the new root
`fspec-esp-sync.service` runs it at boot and again when it stops at
shutdown, ordered after `ostree-finalize-staged.service`, so a staged update
reaches the ESP. The ESP is mounted at `/boot/efi` (`boot-efi.mount`, by
PARTUUID), where `systemd-bless-boot` finds it. Fedora's SELinux policy does
not let `init_t` rename files on the ESP, so the sync and
`systemd-bless-boot.service` run as `unconfined_service_t`.

The new root keeps the machine's identity from the root `--inherit
@<subvolume>` names, by default the running one: its ssh host keys,
NetworkManager connections (mode 0600) and `/var/lib/tailscale` (0700), and
the host's declared users with the source's uid, gid, gecos, home and
password hash. A user's shell is looked up by name in the new root
(`/run/current-system/sw/bin/fish` becomes `/usr/bin/fish`), and the user
joins the source's supplementary groups that the new root has, copying a
group such as `wheel` from `/usr/lib/group` to `/etc/group` first. The
running root's `/home` subvolume goes into the new root's `fstab` as
`/var/home` with the same mount options. The script prints none of what it
copies. A root without SELinux writes no labels, so `setfiles` from the image
labels the deployment's `/etc`, the stateroot's `/var` and the shared home
afterwards; sshd refuses an unlabelled `authorized_keys`.

`--pull-from <repository>` pulls the locked digest from a plain-HTTP registry
(`podman pull --tls-verify=false`, for that pull only) and installs with
`--target-imgref=<locked repository>:latest --skip-fetch-check`, so later
`bootc upgrade` runs go to the locked repository.

A boot is blessed only once the machine can be reached:
`fspec-boot-guard.service` is required by `boot-complete.target` and waits up
to five minutes for an active `sshd.service` with a listening socket and for
`tailscale status` to report this node online, and fails otherwise. With
boot counting active, `fspec-boot-deadline.timer` reboots a boot that is not
blessed ten minutes in, and `RuntimeWatchdogSec=60s` reboots a hung one.
Both units only run while systemd-boot counts tries, so a blessed root that
is offline keeps running. The install enables `sshd` and `tailscaled`.
Making the new root the default is a separate step. Use `preferred
<name>-ostree-*` in `loader.conf` or the `LoaderEntryPreferred` EFI variable,
which skip an entry with no tries left. `default` does not skip it, so a bad
entry there boots again and again.

For a host that declares `tumbleweed()`, `fspec switch --beside <name>`
without `--to-image` bootstraps openSUSE Tumbleweed into `@<name>` the same
way, at the snapshot `fspec.lock` pins (`[zypper.<host>]`). From a live
system, `--device <btrfs partition> --inherit @<root>` names the filesystem
and the root whose identity it keeps, and an ESP that is not mounted is
mounted for the run. zypper runs from the official
`registry.opensuse.org/opensuse/tumbleweed:latest` container (the registry drops old digests,
and the locked snapshot fixes the packages), as
`zypper --root /mnt --gpg-auto-import-keys install --no-recommends` against
the mounted subvolume, so Tumbleweed's own rpm writes the rpmdb; the live
system needs only podman and the network; zypper runs at idle CPU and I/O
priority. The root gets the same
`fspec-oss` and `fspec-non-oss` repos and `fspec_snapshot` variable that
`fspec apply` keeps, so its first apply sees no drift. It gets the minimal
base pattern with zypper itself (the pattern leaves it to recommends, which
the bootstrap skips), `kernel-default`, `kernel-firmware-all`, NetworkManager,
openssh, sudo, the `wheel` group, tailscale, power-profiles-daemon, snapper
with its zypp plugin, zram-generator, tar for the spec a remote apply ships, timezone, and git-core
for `git(...)` builds. `fspec-power-saver.service` picks
the power-saver profile on the first boot.
Swap is zram, not the other root's swapfile, which a Tumbleweed boot would
overwrite while that root is hibernated. `snapper create-config /` puts
`.snapshots` in a subvolume nested in `@<name>`, so the snapshots stay with
the root and every zypper transaction takes a pre and post snapshot. Booting
a snapshot (`snapper rollback`) needs the root to be the btrfs default
subvolume, which stays the top level here, so snapshots are for diffing and
undoing changes, not for booting.

The initrd is dracut's generic one with the btrfs module, built in the
container where host-only detection would see the wrong machine, from the
options in `/etc/kernel/cmdline`: `root=UUID=<fs>
rootflags=subvol=@<name>,noatime,compress=zstd:1 rw panic=10 rd.shell=0
rd.emergency=reboot`, the `console=` options of the inherited root's
entries, then the host's `tumbleweed(kernel_args = [...])`. Apply keeps those
args in the file: it drops the ones it added last time and any option an arg
sets to another value (`mitigations=off` replaces `mitigations=auto`), appends
the current ones, and starts `fspec-esp-sync.service` so the entries carry
them. The base options stay the switch's, and drift lists an arg that went
missing. A spec that wrote the whole file with `file("/etc/kernel/cmdline", ...)`
can switch to `kernel_args` in one apply: a removed `file` whose path another
resource writes is handed over, dropped from the state and left on disk,
because removals commit before the resource that now owns the path. `/etc/fspec/esp-sync.sh` copies each installed kernel to
`<esp>/<name>/<kernel version>/linux` and its initrd to `initrd` beside it,
and writes `<esp>/loader/entries/<name>-<kernel version>+3.conf`; a rewrite
keeps the counter, and the kernels zypper removed go. While an entry still
counts tries, its options mask `emergency.target`, `emergency.service`,
`rescue.target` and `rescue.service`, so a trial boot that fails a mount goes
on to sshd degraded instead of waiting offline at a shell. A drop-in runs the
sync right after `systemd-bless-boot`, which rewrites the now blessed entry
without them, so a blessed root gets its emergency shell back. sdbootutil is not used:
it unlinks entries it cannot parse and manages the default entry and the EFI
boot order. A libzypp commit plugin runs the sync after every transaction,
once the kernel scriptlets have rebuilt the initrds, and
`fspec-esp-sync.service` runs it at boot. When the root has
`/etc/fspec/limine.conf` and the ESP has `EFI/limine/`, the sync also writes
`<esp>/EFI/limine/limine.conf`: that file as the header (theme, the chain entry
to systemd-boot, other systems), then a `Kernels` submenu with one `linux` entry
per boot entry on the ESP, its kernel, initrds and options, booted straight and
without counting tries. A spec themes or extends Limine by writing
`/etc/fspec/limine.conf` with `file(...)`, never the ESP copy;
`fspec-esp-sync.path` reruns the sync whenever that file changes. On a switched
root the sync owns the ESP's `EFI/limine/limine.conf`, so a `file` that wrote it
is handed over on removal and the menu stays on disk until the sync rewrites it.
Apply keeps `esp-sync.sh` and the path unit at the running fspec's version: when
either differs, drift says so, and apply rewrites them (with backups in the
journal), enables the path unit and starts the service. The `file` phase runs
first, so a header written in the same apply is the one the sync reads. The guard, the deadline timer and
the watchdog are the bootc root's, except that the guard waits for tailscale
only when the inherited root had tailscale state: tailscaled writes a state
file of its own before anyone logs in, so the install leaves
`/etc/fspec/needs-tailscale` instead.

The old root stays bootable until `fspec forget <distro>` deletes it. It
refuses the running distro and the root it booted from, and stops when that
root does not exist. It is a hard refusal, not a warning, whenever the running
root does not own boot (the default entry, `bootfs` or the GRUB owner points
at another root, or nothing records an owner); the message says to run `fspec
switch --adopt-boot` from the running root first. That way the bootloader
never goes with a deleted root. It removes the root's kernels and entries from
the ESP or its folder in the shared boot partition (and regenerates GRUB's
menu on BIOS), then deletes the root with `btrfs subvolume delete --recursive`
or `zfs destroy -r`, which takes every leftover system file with it.
`fspec forget nixos` takes the NixOS a migration kept in `@nixos`, with its
`EFI/nixos` folder and `nixos-*.conf` entries. Off the default layout, a root
installed with `--beside` has no boot owner record, so forget checks two
things instead: the running root still has its own entries on the ESP, and
`loader.conf` does not name the root that goes. All of these need root; every step is a pure planning function in `fspec-switch`
that returns the command list.

Only the `system` block differs per distro. Everything from FormalSpec's own
sources, Flatpaks and files moves unchanged. Base packages are named per
distro, so the `system` block is where those names live.

## Installer

The installer puts a FormalSpec-managed system on a blank machine. It is a
Debian live ISO, built with live-build under `installer/iso/`, so it boots
with Secure Boot on through Debian's signed shim. The ISO carries a static
`fspec`, the installer `fspec-install`, and every bootstrapper `disk init`
calls: debootstrap, dnf, pacman with arch-install-scripts, a static xbps, a
static moss and podman. Debian's pacman comes without repositories, so the ISO
adds `[core]` and `[extra]` with a mirrorlist that pacstrap copies into the new
root, and makes the pacman keyring at boot.
`installer/iso/build.sh` builds the ISO; `installer/iso/vm.sh` boots it in a libvirt VM and opens virt-manager.

`fspec-install` is a full-screen terminal program (ratatui). On a machine
with a GPU it runs inside `foot` under a bare `sway` session, which
gives it true colour, a proper font and the mouse for a few megabytes; without
one, or on a serial console, it runs on the console itself in 16 colours. A
graphical toolkit would have cost hundreds of megabytes of ISO and lost the
serial console, which headless servers and VM tests need.

The flow is language and keyboard, disk, distro, preset, account, review,
install. The right side of the screen shows the `spec.star` being written as
choices are made. The disk gets the default layout above, with LUKS2 when
asked. On a disk with Windows (an ESP and an NTFS or BitLocker basic data
partition, read with `sfdisk --json` and `ntfsresize --info`), the disk screen
offers "Install beside Windows", the default, or "Erase the whole disk";
beside Windows it uses the free space, or shows a size picker in decimal GB
(8 GB steps, 128 GB by default) for what Linux takes from `C:`, or the one-line
reason Windows cannot be shrunk with what to do in Windows. The review screen
says how much Windows shrinks, or that it is untouched. The distros are the ones
`disk init` can bootstrap: Fedora, Arch, Ubuntu, Debian, Void and AerynOS, each
tagged with its release model, init system and whether its kernel is
vendor-signed. A distro joins the list only once `disk init` installs it, which
a test over every distro, preset and desktop checks.
The presets are minimal, desktop (GNOME or KDE Plasma), gaming and server,
each a Starlark module in `crates/fspec-install/presets/`. The installer
writes a spec repo (`spec.star`, `modules/<preset>.star`) with one host named
after the machine and its locale, keymap and timezone, commits it, and
installs from it. The user gets `shell = "bash"`, since useradd's default is
`/bin/sh` on Debian, Ubuntu and Void, and the full name from the account
screen through `usermod -c`. It writes no `fspec.lock`: the presets declare
only base packages, which stay unpinned, and Flathub apps that update themselves, so
the first `fspec update` writes one when the spec gains a pinned package.
The repo ends up in `~/spec` of the new user, so `fspec plan` and
`fspec apply` work from the first login.

Restore from git clones an existing spec instead (an SSH key file or an HTTPS
token, which is sent as a header for that one clone and never stored), lists
its hosts, and installs the one picked exactly as declared.

The install itself is `disk init` with the host's distro as the first root
(`fspec_switch::init`), so partitioning, the bootstrap and the first apply
are the same code `fspec disk init --distro` runs. The installer adds only
steps inside the new root before it is unmounted: the user's password through
`chpasswd`, read once from the account screen and never written to the spec,
and the copy of the spec repo into the user's home. Without EFI it picks the
BIOS layout. Each command's output streams to the install screen and to
`/var/log/fspec-install.log`, which stamps each command with the seconds since
the install started.

`fspec-install --answers <file>` takes the choices from a JSON file instead of
the screens (a fresh install onto a whole disk; no restore, no Windows, no
Secure Boot), checks them against the catalog, runs the same install and
prints each stage with how long it took. It is how the ISO is tested in a VM
over the serial console.

Secure Boot on the installed system is optional and off by default, since it
needs the firmware in setup mode; the plan runs no `sbctl` otherwise. Off,
systemd-boot and the unified kernel images are unsigned. On, the plan builds
the new root, then `sbctl create-keys` runs in the live session, the root gets
`sbctl` (the distro's package, or on Debian the ISO's binary) with the keys
copied to `/var/lib/sbctl`, and systemd-boot and every UKI on the ESP (Void's
copied kernel) are signed with `sbctl sign -s` inside the root, so its database
holds the installed paths. `sbctl enroll-keys --microsoft` runs last, keeping
the Microsoft UEFI CA, so a failed install leaves the firmware's keys alone.
Every kernel here is a UKI built on the machine, so a vendor signature would
not survive it: all distros are signed with the machine's key, and there is no
shim chainload. Kernel updates stay signed through a kernel-install plugin
(`91-sbctl.install`: Fedora, Debian, Ubuntu), a mkinitcpio post hook (Arch) or a
kernel.d hook (Void), and a `systemd-boot-update.service` drop-in re-signs
systemd-boot after `bootctl update`. Beside Windows, systemd-boot on Windows'
ESP is signed from the live session, and the drop-in also signs it at
`/boot/efi` with `-s` after each update. AerynOS cannot use the option: moss
writes its own boot files and there is no sbctl package, so its first kernel
update would boot unsigned. The review screen says so and the plan runs no
`sbctl` for it. Measured boot and anti-cheat on a
self-signed chain are unverified.


## Commands

| Command | Does |
| --- | --- |
| `fspec update [pkg] [--cache]` | Resolve upstream, rewrite the lock; `--cache` only records CI-built OCI digests |
| `fspec update --images` | Pin each image host's pushed digest |
| `fspec image containerfile --host h` | Print the Containerfile for a bootc host |
| `fspec build --pkg p --platform t --out d` | Build a `git(...)` package into a `.tar.zst` |
| `fspec ci init [--registry r]` | Write the image build workflow |
| `fspec ci packages` | Write the workflow that builds and pushes patched packages |
| `fspec plan [--all] [--dirty]` | Diff config against state, per host; `--all` asks each remote host over ssh |
| `fspec apply [--host h] [--all] [--dirty]` | Converge, over ssh for remote hosts |
| `fspec rollback [gen]` | Previous generation, data included |
| `fspec dev [-- cmd]` | A shell, or one command, in the nearest `dev.star`'s environment |
| `fspec dev export` / `direnv` | The environment as bash for direnv, and the `use fspec` library |
| `fspec dev up` / `down` | Start or stop the project's services |
| `fspec dev plan` | What the project environment holds and what the lock could not resolve |
| `fspec gc --older-than 14d` | Drop old generations, store paths and trash |
| `fspec adopt <path>` | Pull an app's edit of a managed file into the repo |
| `fspec orphans` | Unattributed app data |
| `fspec watch` | Report drift live |
| `fspec switch --distro d [--release r]` | Install another distro beside this one |
| `fspec switch --adopt-boot` | Make the running root own boot, so the old one can be forgotten |
| `fspec switch --reset-var` | Stage a reset of `/etc` and `/var` for the next boot of a bootc host |
| `fspec switch --to-image --yes` | Convert a classic machine to the host's bootc image |
| `fspec switch --to-image --beside <name> [--inherit @<root>]` | Install the host's bootc image into `@<name>` beside this root |
| `fspec switch --beside <name> [--device <part>] [--inherit @<root>]` | Bootstrap the host's `tumbleweed()` into `@<name>` beside the other roots |
| `fspec forget <distro>` | Delete an old distro root, from the root that owns boot |
| `fspec disk plan [device]` | Steps that bring a disk onto a supported layout |
| `fspec disk migrate [device]` | Run the in-place steps of `disk plan` |
| `fspec disk migrate --stage <iso> --nixos-system <path>` | Boot the live ISO once from the disk and migrate this NixOS root there |
| `fspec disk init <device>` | Partition and format a disk with a supported layout |
| `fspec sync [item...] [--peer h] [--from h] [--rollback] [--dry-run] [--watch [s]]` | Sync app data with the item's peers |

## Sync between hosts

App data a spec cannot declare (a signed-in account, a vault, a wallpaper
folder) moves between the user's own machines with `fspec sync`. It is peer to
peer over ssh: no server, no git remote, no relay. An item names every host
that holds it in `peers`, and each of those hosts lists the same item, in
`host(sync = [...])` or from a module:

```python
PAIR = ["g815", "e1504g"]
sync("wallpapers", peers = PAIR, paths = ["~/Pictures/Wallpapers/"])
sync("active-wallpaper", peers = PAIR,
     keys = json_keys("~/.local/state/formalshell/state.json", ["wallpaper"]),
     after = ["wallpapers"],
     on_receive = "formalshell ipc --any-display call wallpaper set {wallpaper}")
sync("helium", peers = PAIR, paths = ["~/.config/net.imput.helium/"], mode = "seed",
     keyring = [keyring(schema = "chrome_libsecret_os_crypt_password_v2", attrs = {"application": "chromium"})],
     unless_running = ["helium"])
sync("gh", peers = PAIR, keyring = [keyring(schema = "org.freedesktop.Secret.Generic",
     attrs = {"service": "gh:github.com", "username": "FormalSnake"})])
```

An item has the same definition on every peer (paths start with `~/` and
resolve against each host's home), otherwise its sync fails naming both hosts.

| Field | Means |
| --- | --- |
| `paths` | Files, or directories when they end in `/`, under `~/`. Regular files only: symlinks, sockets and empty directories are skipped |
| `include`, `exclude` | Globs on paths inside a directory. A pattern without `/` matches any component (`Cache` drops every `Cache` directory); one with `/` matches from the directory. With `include`, only matching files sync |
| `keys` | `json_keys(file, [keys])`: only these top-level keys of a JSON file sync, the rest of the file stays each host's own |
| `keyring` | Secret Service items: `keyring(schema = ..., attrs = {...}, label = ...)` matches every item with that schema and those attributes (a value ending in `*` matches a prefix), and that label when given. The Secret Service searches by attribute, so a schema or an attribute is required. Chromium and Electron apps encrypt their cookies with one (`Chromium Safe Storage`), so a profile synced without it comes up signed out |
| `mode` | `live` (default): every peer writes. `seed`: copied whole to a host that has none of it, then each keeps its own |
| `conflict` | For `live`: `newest` (default) or `manual` |
| `after` | Items that sync first; this one waits while one of them did not, so `on_receive` finds the file the key points at |
| `on_receive` | A shell command run on a host after the item changed there; `{key}` is that key's new value, shell-quoted |
| `unless_running` | Process names that hold the item open; while one runs on either host the item is skipped |

**Modes.** A `live` item merges per file, keyring item and JSON key against
the base, what both hosts held at the end of their last sync. Changed on one
side only, it is copied, a deletion included. Changed on both, `newest` takes
the later modification time and a change beats a deletion, while `manual`
leaves both copies and `fspec sync` exits non-zero naming them. A keyring item
is never deleted. `seed` is for single-writer data (LevelDB and SQLite
profiles, OAuth refresh tokens that rotate): copied whole, never merged, to a
host that has none of it. Once both hold it, `fspec sync helium --from g815`
hands it over, replacing the other host's copy (`--from` on a `live` item does
the same). Data bound to one device is not synced at all, and each host links
itself: Flare (a Signal linked device), Beeper (Matrix device keys), 1Password
(a registered device).

**Transport.** A peer is reached over the routes of this host's builder of
the same name when it declares one (the Tailscale name, then the LAN), else
over its `ssh` alias, probed like a remote apply: it must run the platform its
host declares and an fspec at least as new as this one. The distro is not
checked, on either end, since an item touches only the home: a host declared
as the distro it is moving to still syncs from the one it runs. An unreachable
peer is skipped. The far end runs `fspec sync-agent`, one request per ssh
call on stdin. Files travel as tar inside ssh, whose host keys authenticate
both ends and encrypt the stream. Keyring secrets are read and stored with
`secret-tool` (libsecret, the `secret-tool` package on Tumbleweed) and travel
only through the ssh session's pipes, never through a file, so the login
keyring must be unlocked on both hosts; a locked one fails the item.

**State and rollback.** Each host keeps, per item and peer, the base in
`~/.local/state/fspec/sync/<item>/peers/<peer>.json`: hashes, sizes and
modification times, never contents or secrets (JSON key values excepted). A
file whose size and modification time match the base is not hashed again, and
a copy keeps the sender's modification time. Before a host replaces or
deletes anything it moves it into `<item>/prev/`, and a replaced keyring item
stays in the keyring under `fspec-prev:` attributes, which match no app's
lookup. `fspec sync <item> --rollback` swaps the previous version back on this
host and forgets the bases, so the next sync merges from scratch.
`fspec sync --dry-run` prints what would move, `--peer` limits it to one peer,
and `fspec sync --watch [seconds]` syncs again every 60 seconds (or the given
interval) until stopped, for a user `service(...)` to run.

## The engine

fspec installs itself. Evaluation adds one package to every Linux host that
does not declare a `pkg("fspec")` of its own: `git(...)` of this repository at
`main`, built static against musl in a pinned `rust:alpine` image so one
binary runs on every distro, with a link from `/usr/local/bin/fspec` to
`/opt/fspec/current/bin/fspec`. Nothing in a spec adds it, so nothing in a
spec can drop it by accident.

It is an ordinary `git(...)` package from there on. `fspec update` locks the
newest commit of `main` that touches `crates/`, `Cargo.toml` or `Cargo.lock`
(`git(..., paths = ...)`): the kyan lock lives on the same `main`, and locking
the tip would move the engine with every lock commit, past what CI has built.
From there, `fspec ci packages` gives it a CI job, `--cache`
records its OCI digest, and `apply` installs it, so a host runs the engine
the lock names from its next apply on, and `fspec rollback` takes the engine
back with the rest of the generation. `/usr/local/bin/fspec` is the path
`fspec switch` and `fspec disk init` install, and the one timers and remote
applies run, so all of them follow the lock. A host whose fspec predates the
engine package gets it from a remote apply ([Remote hosts](#remote-hosts)).
Until the lock pins the engine, plan and apply leave it and its link out and
the host keeps the fspec it runs: the repo the installer writes has no
`fspec.lock`, and its first apply runs in the new root before any
`fspec update`.

macOS hosts are left out: the macbook's fspec comes from Nix.

## Remote hosts

A host is remote when its name is not this machine's short host name. It is
reached by the ssh alias in its `ssh` field (`host(name = "g815", ssh =
"kyan@g815.lan", ...)`), which defaults to the host name, so `~/.ssh/config`
decides keys, ports and jump hosts.

`fspec apply --host h` on a remote host does four things, every command going
through the `Runner`:

1. Probe: `ssh -o ConnectTimeout=5 -o BatchMode=yes h fspec facts --json`
   prints the machine's platform and distro. They must equal what the
   declaration implies, otherwise apply fails before touching anything:
   `host g815 is declared as x86_64-linux/bazzite but the machine is
   aarch64-linux/fedora`. The probe is a check, never an input to evaluation.
   A missing `fspec` on the remote fails with an install hint. The probe also
   prints the remote fspec's engine number, raised with every builtin, field
   or lock key a spec can use; one lower than this fspec's fails the same way
   (`fspec on g815 is older than this one (engine 0 < 1)`), since it may
   reject the spec or ignore lock keys it does not know, unless the lock holds
   a CI build of [the engine](#the-engine) for the remote's platform. Then the
   driving fspec pulls it, streams it to `~/.cache/fspec/engine.new` and runs
   `ssh -t h sudo install` to put it at `/usr/local/bin/fspec`, probes again
   and carries on. It never ships its own binary, which may not run there.
2. Refuse a dirty spec tree. Hosts that apply the same commit end up with the
   same software, so uncommitted changes under the spec directory stop the
   apply unless `--dirty` is passed.
3. Ship: `git ls-files` in the spec directory plus `fspec.lock` go through
   `tar` and ssh into `~/.cache/fspec/spec/<commit>/` on the remote. A dirty
   tree also ships its untracked files that git does not ignore, and lands in
   `<commit>-dirty-<hash>/`, named after a hash of what is shipped, so a
   second edit at the same commit never evaluates the first one's copy; the
   earlier dirty copies of that commit are deleted first. The `gh:` load files the
   lock names follow from this machine's cache into `<dir>/.fspec/loads`, so
   the remote needs no GitHub token to evaluate them.
4. Run `ssh -t h fspec --spec <dir>/spec.star apply --local --host h` with the
   terminal handed over, so sudo can prompt. `--local` stops the remote end
   from forwarding the apply again if its host name differs from the declared one.

`fspec apply --all` applies the remote hosts in spec order, then this machine,
and stops at the first failure with the hosts that converged and the ones not
attempted. `fspec plan --all` probes each host: a reachable remote host is
shipped the spec and runs `fspec plan --json --host h`, whose plan is rendered
locally; an unreachable one prints `unreachable, skipped`, since its state lives
on the host. Runs under `--root` never go remote.

## Repo layout

```
spec.star          entry point, returns `hosts`
modules/           one concern per file
files/             static config files
patches/           .patch files referenced by packages
fspec.lock         generated, committed
```

## Implementation

A cargo workspace under `crates/`:

| Crate | Holds |
| --- | --- |
| `fspec-core` | Resource types, host facts, `Paths`, the state file, the lock, the plan diff, the apply engine, the journal, the `Runner` and `Fetch` seams |
| `fspec-eval` | Starlark builtins and evaluation into `Host` values |
| `fspec-files` | The `file` and `secret` backend: the four modes, format writers, the three-way patch merge, `adopt`, `watch` |
| `fspec-store` | The store backend: hash check, unpack, generations, `current`, npm and nix packages, project unpacks, `gc` |
| `fspec-dev` | `dev.star` projects: locking, `lib()`/`bin()` resolution, activation, containers, services |
| `fspec-image` | Containerfile and workflow generation, ghcr digest resolution, the bootc `image` backend |
| `fspec-sources` | Resolvers (aqua, github, npm, cargo, flatpak, brew, git, nix, dnf, pacman, apt, xbps) and `fspec update` |
| `fspec-pkgmgr` | Flatpak, Homebrew and the distro package backends (dnf, pacman, apt, xbps), and the base packages of a classic distro |
| `fspec-os` | Services and timers per init (systemd, launchd, runit, OpenRC, dinit, s6, cron), users, macOS `defaults` and `setting` |
| `fspec-remote` | Remote hosts: ssh and tar command lines, the spec shipment, the probe check, `--all` ordering |
| `fspec-switch` | `fspec switch` and `forget`: btrfs and bootloader detection, the boot owner record, the command plans per distro and for `--adopt-boot`, the `--reset-var` staging |
| `fspec-sync` | `fspec sync` and `sync-agent`: scanning items into manifests, the three-way merge, tar and keyring transport over ssh, `prev` and `--rollback` |
| `fspec-install` | The installer |
| `fspec-build` | Source builds: recipe detection, the `.tar.zst` packer, builders over ssh with the local fallback, the `ci packages` workflow, `update --cache` |
| `fspec` | The CLI |

Builtins validate their arguments against the `fspec-core` types as soon as
they run, so an unknown field fails at the line that wrote it. State keys are
`kind:id` (`pkg:ripgrep`, `file:~/.config/fish/config.fish`); each entry keeps
the resource as last applied, the package that owns it, and a `record` object
whose shape belongs to the backend that applied it.

Backends implement one trait: `handles` claims resources on a host (exactly
one backend per resource, or `plan` lists it as unsupported and `apply`
refuses), `drift` reports live divergence for unchanged resources, `prepare`
fetches and renders into a staging directory (the store also checks there that
a package holds the `bins`, `links`, `share`, `etc` and `apps` the spec names,
so a wrong one fails before any phase commits), and `commit` makes changes live,
journaling an undo step before each mutation. A backend can name the live
paths a resource writes (`writes`); a removed `file` at one of those paths is
handed over instead of deleted, and the plan shows it as `>` rather than `-`.
A file whose `src` moved to another path with the same bytes is checked for
drift against its new source. The engine commits removals in
reverse phase order, then adds and updates in phase order (users, files, store,
packages, services, defaults, system). A backend can then `settle` its adds
and updates, in the same phase order, for work that needs what later phases
installed (group membership, a link to a target a package or service creates),
and `refresh` the entries no change touched,
for what a change moved under them (a service running a replaced store
package). The engine writes the new state and
`generations/<n>.json` only after every commit succeeded. Each scope (system,
user) is its own transaction with its own journal. A non-root `fspec apply`
runs the system scope through `sudo fspec apply --scope system`, then the user
scope as the user. Every external command goes through a `Runner` and every
HTTP read through `Fetch`, so tests record them instead of touching the
machine or the network. With `--root` set to anything but `/`, the CLI logs
commands to `<root>/var/log/fspec-commands.log` instead of running them, and
`Fetch` also reads `file://` URLs for fixtures.

Every path goes through `Paths`, rooted at `--root` (default `/`) with the
home at `--home`, so tests and dry runs work in a temp directory and never
touch the live system.

## Build order

All eight steps are implemented and tested against temp roots with recorded
commands. Steps 1 to 6 have also run live on macOS and on the four classic
distros; bootc and `fspec switch` have not.

1. Starlark evaluation, resources, state file, `plan`.
2. `file` with all four modes, three-way merge for `patch`.
3. Store, generations, rollback; aqua, github and npm sources; `update` and the
   lock.
4. Flatpak, brew, dnf backends; systemd and launchd services.
5. Removal with data trash and `gc`.
6. Remote apply over ssh.
7. `image` with bootc, the OCI cache for patched builds.
8. Other distros and inits, `fspec switch`.

Steps 1 to 4 replace a home-manager setup on a single machine and are the
point where FormalSpec becomes usable day to day.

## First release

v0.1 is the release that removes Nix from Kyan's machines: openSUSE
Tumbleweed under FormalSpec replaces NixOS on the g815 and the e1504g, and
FormalSpec over Homebrew replaces nix-darwin on the macbook. The design session
decides when it is ready, and every item below is required.

The spec is the `formalspec` branch of
[FormalSnake/dotfiles](https://github.com/FormalSnake/dotfiles/tree/formalspec),
ported from `~/.config/nix`, which the port reads and never edits. Its
`GAPS.md` lists everything not ported and why.

The e1504g already runs Tumbleweed from that spec and is applied from the
macbook. The g815 still runs NixOS: UEFI with Secure Boot off, an ext4 root on
`/dev/nvme0n1p5`, a 1 GB EFI partition mounted at `/boot`, and Windows on NTFS
partitions p3, p4 and p6 with its boot entry on the same EFI partition.

Done:

- **Unattended migration from NixOS on ext4.** `fspec disk migrate --stage`
  converts the root to btrfs, keeps NixOS in `@nixos` and rolls back to ext4
  on failure; proven in a VM.
- **Install beside and rollback.** `fspec switch --beside` installs
  Tumbleweed next to `@nixos`, and a failed boot falls back; proven in a VM.
- **A recovery page** covers booting back into NixOS and undoing the
  migration.
- **CI.** It runs the tests, builds `git(...)` packages into the OCI cache
  (FormalShell, librepods) and opens an `fspec update` PR on demand. The update
  runs on `workflow_dispatch` only, since GitHub schedules run from the default
  branch and the spec lives on `formalspec`.

Open:

1. **Parity gaps.** Drop dcal, which FormalShell cannot read, and apply and
   check uxplay, colloid-icon-theme and Discord with moonlight, which no host
   has applied yet.
2. **The g815 port**, in this order:
   - port the macOS 10.9 guest: OpenCore from its release zip, the
     config.plist render and ESP image in the launcher;
   - build a NixOS system that mounts `/` as `subvol=@nixos`, `/home` from
     `@home` and `/swap` from `@swap`, take a backup, and run `fspec disk
     migrate --stage <iso> --nixos-system <path>`. It converts p5 in place;
     the partition table, Windows' partitions and its boot files stay as
     they are;
   - after the reboot into `@nixos`, run `fspec switch --beside tumbleweed`;
   - boot Tumbleweed and check the Limine menu still boots NixOS and Windows,
     the NVIDIA G07 driver, the gpu-mux service, the NTFS mounts and the
     macOS guest.

   Swap is zram while `@nixos` exists; the 16G swapfile in `@swap` comes back
   after `fspec forget nixos`. Not yet proven on real hardware: the 1 GB EFI
   partition holding the g815's NixOS closure.
3. **Remote convergence.** `fspec apply --all` from the macbook converges both
   Linux hosts over ssh, and `fspec plan --all` shows no drift afterwards.
4. **The release ISO workflow** runs once and publishes an ISO.
5. **The macbook port.** `fspec plan --host macbook --local` evaluates clean.
   Drop the skill copies `meshtop skills sync` already installs. Apply it
   beside nix-darwin, which renames each managed file over Home
   Manager's `/nix/store` link; unload nix-darwin's launchd jobs (`kyan.*`,
   `org.nix-community.home.*`), whose `fspec.*` replacements the apply
   starts; then check the CI-built packages (pam_ssh_agent_auth, sbarlua,
   Messages, FormalMusic) and the sudo mesh.
6. **Nix removed.** `fspec forget nixos` on both Linux hosts, nix-darwin's
   uninstaller and then Determinate Nix's `/nix/nix-installer uninstall` on
   the macbook. The host-ops skill and the rules that name `~/.config/nix`
   point at the spec instead, and `~/.config/nix` is archived.

Rollout of the g815 starts once 1 and 4 pass and the VM steps of 2 have run
against a copy of its disk layout. Every step of 6 deletes the way back, so
each runs only after Kyan says so, and the g815's real disk is not touched
before the design session gives the go.
