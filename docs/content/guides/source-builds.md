+++
title = "Building from source"
description = "Build git(...) packages at a locked commit, patch packages, and serve the builds from CI or a builder machine."
+++

A `git(...)` package is built from a repository at a locked commit, with a recipe FormalSpec
picks from the checkout. Use it for software that no distro packages and no release ships: your
own projects, a fork, a tool that only publishes source. Everything else comes faster from the
distro or a prebuilt release; see [choosing a source](packages.md#choose-a-source).

A build runs in one of three places, in this order: a CI job that already pushed the artifact,
a builder machine you name, or the host itself.

## Build a package from git

1. Declare the package with a `git` source. `rev` is a branch, a tag, `latest` or a full commit:

   ```starlark
   pkg("hyperfine", source = git("https://github.com/sharkdp/hyperfine", rev = "v1.19.0")),
   ```

2. Lock the commit:

   ```console
   $ fspec update
   + hyperfine 12fec4209864
   ```

   The lock now holds the exact commit:

   ```toml
   [[pkg]]
   name = "hyperfine"
   source = "git:https://github.com/sharkdp/hyperfine@v1.19.0"
   rev = "12fec42098642a19855ead34c8cb1e0be28c8ead"
   ```

3. Apply. The host checks out that commit, builds it, and installs the result into the store
   like any other package:

   ```sh
   fspec apply
   ```

| `rev` | Locks |
| --- | --- |
| `"main"` (default), any branch | The branch's current commit. Each `fspec update` moves it |
| a tag, such as `"v1.19.0"` | The commit the tag points at |
| `"latest"` | The highest version tag by version sort, prereleases included. The lock also records the tag as the version |
| 40 hex digits | That commit. `fspec update` never moves it |

`fspec update` resolves the ref with `git ls-remote`. `apply` never does, so the build always
uses the locked commit.

`paths` narrows a branch to the parts of the repository that make the build: with
`git(url, paths = ["crates", "Cargo.lock"])`, `fspec update` locks the newest commit on the branch
that touches one of them, read from a clone of the commits and trees without file contents. A
commit that only changes docs then leaves the lock, and the cached build, where they are. Tags
and full commits ignore `paths`.

## Recipes

`fspec build` checks out the locked commit, applies the patches, and picks the first recipe whose
file the checkout has:

| Recipe | Detected by | Commands |
| --- | --- | --- |
| Cargo | `Cargo.toml` | `cargo build --release --locked --manifest-path <src>/Cargo.toml --target-dir <build> <build_flags>` |
| Meson | `meson.build` | `meson setup <build> <src> --buildtype release --prefix <out> <build_flags>`, `meson compile -C <build>`, `meson install -C <build>` |
| CMake | `CMakeLists.txt` | `cmake -S <src> -B <build> -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=<out> <build_flags>`, `cmake --build <build> --parallel`, `cmake --install <build>` |
| Go | `go.mod` | `go build -C <src> -o <out>/bin/ <build_flags> ./...` |
| Make | `Makefile`, `makefile` or `GNUmakefile` | `make -C <src> <build_flags>`, `make -C <src> install PREFIX=<out>` |
| explicit | `build = [...]` on the `pkg` | your command |

What ships:

- A Cargo build ships the executables from `<build>/release`.
- Every other recipe installs into `<out>`, a prefix, and the whole tree ships: `bin/`, data in
  `share/`, helpers in `lib/`. The commands in `bin/` are linked onto `PATH`.

### Pass flags to the recipe

`build_flags` are appended to the recipe's configure or build command:

```starlark
pkg("librepods", source = git("https://github.com/example/librepods"),
    build_flags = ["--features", "pipewire"]),
```

### Write your own build command

`build` replaces recipe detection. It runs with two variables set: `FSPEC_SRC`, the patched
checkout, and `FSPEC_OUT`, where you put the result. Executables go in `$FSPEC_OUT/bin`.
`build_flags` do not apply.

```starlark
pkg("fetch", source = git("https://github.com/example/fetch", rev = "7b19d22c2d8e4b7b2295cc625c9fdf87adcabbcd"),
    build = ["sh", "-c", "cd $FSPEC_SRC && zig build -Doptimize=ReleaseSafe --prefix $FSPEC_OUT"]),
```

### Choose which commands ship

`bins` keeps only the named executables, by file name. The default is every executable the
recipe produced:

```starlark
pkg("ananicy-cpp", source = git("https://gitlab.com/ananicy-cpp/ananicy-cpp", rev = "..."),
    bins = ["ananicy-cpp"]),
```

## Patch a package

`patches` lists patch files in your spec repository, applied in order. `glob` collects them,
sorted, so a numbered prefix sets the order:

```starlark
pkg("pam_ssh_agent_auth",
    source = git("https://github.com/example/pam_ssh_agent_auth", rev = "..."),
    patches = glob("patches/pam_ssh_agent_auth-*.patch")),
```

1. Put the patch in the repository, for example `patches/pam_ssh_agent_auth-01-key-files.patch`.
2. Lock its hash:

   ```sh
   fspec update
   ```

   The lock entry gets `patches = "sha256-..."`, a hash over the patch names and contents in the
   order the spec lists them.

3. Apply. A git package applies the patches with `git apply` before the recipe runs.

When a patch file changes and you have not run `fspec update`, the package is refused with
`patches changed, run fspec update`: at `apply`, and as a drift note in `plan`.

`glob(pattern)` only reads the spec's own tree. A pattern cannot start with `/` or contain `..`.

### Patches on downloaded packages

Patches also work on packages that ship as an unpacked tree: npm packages and release archives.
They need no build. FormalSpec applies them itself before the tree gets its store path, to:

- `package/` for an npm tarball, or `node_modules/<name>` after an `npm install`,
- the archive's single top-level directory, for any other archive.

```starlark
pkg("vtsls", version = "0.3.0", source = npm("@vtsls/language-server"),
    patches = ["patches/vtsls-no-telemetry.patch"]),
```

These patches are git or unified diffs with `a/` and `b/` prefixes. They can edit text files,
create, delete and rename files. A rename with no changes moves any file, binaries included.
Binary diffs are not supported. The store directory name includes the patches' hash, so a patched
tree never shares a path with the unpatched one.

## Build in a container

A build on the host itself has only what the host has installed, and the binary has to load on
the hosts that run it. `image` runs the whole recipe in a container of the distro you name, and
`deps` lists the packages the build needs there:

```starlark
pkg("uxplay",
    source = git("https://github.com/FDH2/UxPlay", rev = "df67c212a433cf6dda3676dd40c097900d24e645",
        image = "registry.opensuse.org/opensuse/tumbleweed",
        deps = ["cmake", "make", "gcc-c++", "pkgconf", "libopenssl-devel", "gstreamer-devel"]),
    bins = ["uxplay"]),
```

- The checkout and patches happen on the build machine. The recipe then runs as one
  `podman run --rm` of the image (`docker run` where podman is missing), with the work directory
  mounted at its own path.
- `deps` install first with whichever of zypper, dnf, apt-get, pacman, xbps-install or apk the
  image has, so you name them the way that distro does. `pkgconfig(gtk4)` works on zypper and dnf.
- Build in the same distro your hosts run. The binary then links against the same library
  versions the host has.
- Pin the image by digest (`...@sha256:...`) for builds that repeat exactly.

On Linux, `deps` without `image` fails the plan. On macOS there is no container of its kind, so
`image` fails the plan and `deps` are Homebrew formulae, installed with `brew install` before the
recipe runs:

```starlark
pkg("tool", source = git("https://github.com/example/tool", deps = ["go"])),
```

## Build by hand

`fspec build` builds one package on this machine into a `.tar.zst`:

```sh
fspec build --pkg hyperfine --platform aarch64-darwin --out out
```

It prints the archive's path, `out/hyperfine-aarch64-darwin.tar.zst`, and writes `out/tag` with
`<rev>-<platform>`. The archive has the executables at the top, with sorted names and zeroed
owners and times. `--platform` must be this machine's platform. `--host laptop` builds on that
host's builders first.

You rarely need it: `apply` builds what it needs, and the CI workflow below runs `fspec build`
for you.

## Builders

A builder is another of your machines that builds `git(...)` packages for a host, over ssh. Use
one when the host is slow, or has no compiler:

```starlark
host(
    name = "laptop",
    platform = "x86_64-linux",
    system = tumbleweed(),
    builders = [builder("desktop", routes = ["desktop", "192.168.1.20"])],
    modules = [...],
)
```

`routes` are ssh destinations for the same machine, in order of preference, such as a Tailscale
name and then a LAN address. Without them, the builder's name is the route.

When an apply needs a package the lock has no CI artifact for:

1. fspec probes each route with `ssh -o ConnectTimeout=5 -o BatchMode=yes <route> uname -sm` and
   keeps the first that answers for the rest of the run. A builder of another platform is
   skipped.
2. It copies itself, a job file and the package's patches to
   `~/.cache/fspec/build/<name>-<rev>` on the builder and runs `fspec build --job` there.
3. It copies the `.tar.zst` back over ssh, deletes the directory on the builder, and installs the
   package on the host.

Nothing is installed on the builder. It needs git, ssh and the recipe's tools, plus podman or
docker for packages with `image`. When fspec runs as root through sudo, the ssh connection runs as
the user who called sudo, so your own keys reach the builder.

When every builder is unreachable or fails, the host builds the package itself and says so:

```console
fspec: builder desktop is not reachable over desktop, 192.168.1.20
fspec: no builder built hyperfine; building it on this host
```

## Serve builds from CI

A CI workflow can build every `git(...)` package once per platform and push the result to an OCI
registry. Hosts then pull the finished artifact instead of building.

Prerequisites:

- The spec repository is on GitHub, with `origin` pointing at it.
- `oras` on every host that pulls artifacts.

1. Write the workflow:

   ```sh
   fspec ci packages
   ```

   It writes `.github/workflows/fspec-packages.yml` at the repository root: a matrix with one job
   per `git(...)` package and platform that a host declares. Each job installs fspec and oras,
   runs `fspec build` and pushes the archive with `oras push`. It runs when `fspec.lock` or
   `patches/` changes on the branch you ran it from (`main` when HEAD is detached), so a spec kept
   on a branch of its own, such as a dotfiles repo's `formalspec` branch, rebuilds from that branch.

   | Platform | Runner |
   | --- | --- |
   | `x86_64-linux` | `ubuntu-24.04` |
   | `aarch64-linux` | `ubuntu-24.04-arm` |
   | `aarch64-darwin` | `macos-26` |

2. Commit and push the workflow, then wait for the jobs to finish.
3. Record the artifacts' digests in the lock:

   ```sh
   fspec update --cache
   ```

   For each git package and platform it reads `<registry>/<name>:<rev>-<platform>` with
   `oras manifest fetch --descriptor` and records the digest:

   ```toml
   [pkg.x86_64-linux]
   oci = "ghcr.io/alex/fspec-cache/hyperfine@sha256:..."
   ```

4. Commit the lock and apply. The host pulls the artifact with `oras pull`, checks it against
   the digest, and installs it.

The registry defaults to `ghcr.io/<origin owner>/fspec-cache`. `--registry` sets another one:

```sh
fspec update --cache --registry ghcr.io/alex/builds
```

A plain `fspec update` keeps the recorded digests while the commit and the patches stay the same.
When either changes, the entry loses its digest until CI has built the new commit and you run
`fspec update --cache` again. Until then, hosts build it on their builders or themselves.

Run `fspec ci packages` again when the set of git packages or platforms changes.

### Registry layout

Each package is one repository, `ghcr.io/<owner>/fspec-cache/<name>`, with one tag per commit and
platform, `<rev>-<platform>`. Each tag is a one-layer artifact whose layer is the `.tar.zst`, with
media type `application/vnd.fspec.package.v1.tar+zstd`. The lock pins the manifest digest, never
the tag.

## Ship a recipe with your project

A project can keep its own FormalSpec recipe in its repository, so users load it instead of
writing one. A `gh:` load reads a `.star` file from a GitHub repository at a locked commit:

```starlark
load("gh:alex/notes-app@main//fspec.star", "notes_app")

hosts = [
    host(name = "laptop", platform = "x86_64-linux", system = fedora(), modules = [notes_app()]),
]
```

Inside a loaded file, `origin()` returns the repository and the commit the file was read from:
`struct(url = "https://github.com/<owner>/<repo>", rev = "<locked commit>")`. In your own spec
files it returns `None`. The project's `fspec.star` can then build itself at that commit:

```starlark
def notes_app():
    o = origin()
    return pkg("notes-app",
        source = git(o.url, rev = o.rev, image = "registry.fedoraproject.org/fedora:44",
            deps = ["cargo", "gtk4-devel"]),
        bins = ["notes-app"])
```

One lock entry then pins, or tracks, both the recipe and the build. `@main` tracks the branch on
each `fspec update`, a tag tracks the tag, and a 40-digit commit pins. Paths in a loaded file
(`glob`, `src`, `patches`) are relative to the user's spec, not to the project's repository.
[Hosts and modules](../concepts/hosts-and-modules.md) covers `gh:` loads in full.

## How it works

- The store directory of a built package hashes the URL, the commit, the patches and the recipe
  (`build`, `build_flags`, `image`, `deps`). Changing any of them builds anew; nothing else does.
- An apply takes a git package from, in order: the lock's `oci` artifact, the host's builders in
  the order listed, then the host itself.
- A git package has no downloaded archive, so it takes no `parts` and no `version`.

## Troubleshooting

| Message | Fix |
| --- | --- |
| `no Cargo.toml, meson.build, CMakeLists.txt, go.mod or Makefile in the checkout, set build = [...]` | The checkout has no recipe FormalSpec knows. Write a `build` command |
| ``git(deps = ...) needs image = ... to install them into`` | On Linux, `deps` install into a container. Add `image` |
| ``git(image = ...) builds in a Linux container; on macOS deps install with brew`` | Drop `image` from a package on a macOS host |
| `patches changed, run fspec update` | A patch file changed after the last `fspec update` |
| ``patch `<path>` must be a path inside the spec's directory`` | Patch paths are relative to the spec and cannot leave it |
| `binary patches are not supported` | A patch on a downloaded package carries a binary diff |
| `the build produced no bin directory`, `no executables in <dir>` | A Cargo or explicit build left no executables where FormalSpec looks. An explicit build puts them in `$FSPEC_OUT/bin` |
| `the build installed nothing into <dir>` | The recipe's install step wrote nothing into the prefix |
| ``` `git ls-remote <url> <rev>` found no matching ref ``` | The branch or tag does not exist |
| `this machine is <platform>, build <platform> on a matching runner` | `fspec build --platform` must match the machine it runs on |
| ``origin is `<url>`, not a GitHub remote, pass --registry`` | `fspec update --cache` cannot derive the registry. Pass `--registry` |
| `no host declares a git(...) package, there is nothing to cache` | `fspec ci packages` found no git packages |
| ``` `<ref>` is not pinned to a digest, run fspec update --cache ``` | An `oci` entry lacks its digest. Run `fspec update --cache` |
