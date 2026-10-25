+++
title = "The system layer"
description = "Declare a host's kernel, bootloader and base packages as a bootc image or as a classic distro's packages, and how each one pins and rolls back."
+++

The system layer is the part of a Linux machine below your packages: the kernel, the bootloader,
the drivers and the distro's base packages. Every Linux host declares it in its `system` field,
in one of two shapes:

- `image(...)`: the whole system is a bootc container image. CI builds it, the lock pins it by
  digest, and apply switches the machine to it. For Fedora Atomic, Bazzite, Aurora and other
  bootc images.
- A distro builtin such as `fedora(...)` or `tumbleweed(...)`: fspec drives the distro's own
  package manager on the running system.

macOS hosts have no `system` field. Their system settings are `defaults`, `setting`, launchd and
Homebrew ([Users and macOS](users-and-macos.md)).

## Choose image or classic

Pick the shape your machine already runs, or the one you want to move to
([Switching distros](switching-distros.md)).

| `system =` | Package manager | Init | What the lock pins | Rollback | Secure Boot and anti-cheat |
| --- | --- | --- | --- | --- | --- |
| `image(...)` | bootc, with dnf inside the image | systemd | The image digest | `bootc rollback`, or the previous deployment in the boot menu | Vendor-signed shim. Fedora and Universal Blue are on EAC's trust list |
| `fedora()` | dnf | systemd | Nothing for base packages; `pkg(source = dnf())` pins the `.rpm` | fspec's undo, plus a btrfs snapshot | Vendor-signed. On the EAC list |
| `arch()` | pacman | systemd | Nothing for base packages; `pkg(source = pacman())` pins the `.pkg.tar.zst` | fspec's undo, plus a btrfs snapshot | Kernel not vendor-signed. Not on the EAC list |
| `debian()` | apt | systemd | Nothing for base packages; `pkg(source = apt())` pins the `.deb` | fspec's undo, plus a btrfs snapshot | Vendor-signed. On the EAC list |
| `ubuntu()` | apt | systemd | Same as Debian | fspec's undo, plus a btrfs snapshot | Vendor-signed (Canonical). On the EAC list |
| `void()` | xbps | runit | Nothing for base packages; `pkg(source = xbps())` caches the `.xbps` | fspec's undo, plus a btrfs snapshot | Kernel not vendor-signed |
| `tumbleweed()` | zypper | systemd | The whole distro at one dated snapshot | Back to the snapshot the apply started from, plus a btrfs snapshot | Vendor-signed (openSUSE). On the EAC list |
| `aerynos()` | moss | systemd | Nothing; moss installs by name from its stream | The moss state from before the apply | Kernel not vendor-signed |

"fspec's undo" is the journal every apply keeps: a failed apply reverses the package changes it
made. `fspec rollback` re-applies an earlier generation
([Removal and rollback](removal-and-rollback.md)). The btrfs snapshot is taken only when `/` is
btrfs ([btrfs snapshots before system changes](#btrfs-snapshots-before-system-changes)).

### Secure Boot and signed kernels

Some games' anti-cheat checks that the kernel was signed by a key it trusts. Epic's Easy
Anti-Cheat Linux v2 trust list (October 2026) includes Fedora, Universal Blue, Canonical, Debian,
openSUSE and Rocky. It does not include Arch, CachyOS or NixOS. If you play such games, pick a
distro from the list.

fspec does not change firmware settings. Secure Boot and the TPM stay in your firmware setup.
`image(kernel_args = [...])` sets the kernel arguments a game or driver needs.

## Image hosts

An image host's system is a bootc image that FormalSpec builds from a base image. bootc keeps the
running deployment and stages the new one, so an update is one switch and one reboot, and the
previous image stays bootable.

```starlark
host(
    name = "desktop",
    platform = "x86_64-linux",
    system = image(
        base = "ghcr.io/ublue-os/bazzite-nvidia:stable",
        packages = dnf(["asusctl", "tuned-ppd"]),
        kernel_args = ["iommu=force"],
    ),
    users = [user("alex", uid = 1000, shell = "fish")],
    modules = [...],
)
```

[`image`](../reference/builtins.md#image) also takes COPR projects, `.repo` URLs, extra
Containerfile stages and steps. `packages` must be `dnf(...)`, and the base must be a dnf distro.

The image is built in GitHub Actions and pushed to the GitHub container registry (ghcr.io). The
lock records its digest. Your machine never builds the image.

### Set up an image host

1. Declare the host with `system = image(...)` as above and commit it. The spec repository must
   be on GitHub.

2. Check the Containerfile fspec renders for it:

   ```console
   $ fspec image containerfile --host desktop
   FROM ghcr.io/ublue-os/bazzite-nvidia:stable
   RUN dnf install -y asusctl tuned-ppd && dnf clean all
   RUN mkdir -p /usr/lib/bootc/kargs.d && printf '%s\n' 'kargs = ["iommu=force"]' > /usr/lib/bootc/kargs.d/fspec.toml
   RUN bootc container lint
   ```

   The kernel arguments go into `/usr/lib/bootc/kargs.d/fspec.toml`, where bootc reads them.
   `bootc container lint` runs last and fails the build on an image bootc cannot boot.

3. Write the workflow that builds it:

   ```console
   $ fspec ci init
   wrote /home/alex/spec/.github/workflows/fspec-images.yml
   ```

   The registry is `ghcr.io/<owner>`, with the owner read from the `origin` remote. Pass
   `--registry ghcr.io/<owner>` to choose another one.

4. Commit and push the workflow:

   ```sh
   git add .github/workflows/fspec-images.yml
   git commit -m "ci: build image hosts"
   git push
   ```

5. Wait for the `fspec-images` workflow to finish. Its first job builds each image host with
   podman and pushes `ghcr.io/<owner>/fspec-<host>:latest`. Its second job runs
   `fspec update --images` and opens a pull request that pins the new digests in `fspec.lock`.

6. Make the image packages public in the package settings on GitHub. `fspec update --images`
   reads the digest without logging in, so it cannot see a private package.

7. Merge the pull request, and pull it on the machine:

   ```sh
   git pull
   ```

8. Apply:

   ```console
   $ fspec apply
   host desktop
     + image:system  ghcr.io/ublue-os/bazzite-nvidia:stable
     ...
   ghcr.io/alex/fspec-desktop@sha256:6f1c... is staged: reboot to activate it
   ```

9. Reboot. The machine boots the new image. The previous one is still in the boot menu.

The workflow runs on every push to `main` that touches a `.star` file, `files/` or `patches/`,
every Monday, and when you start it by hand. Each run opens or updates the same pull request,
on the branch `fspec/image-digests`. Run `fspec ci init` again after you add an image host, so
the workflow builds it too.

The pull request needs Actions to be allowed to create pull requests in the repository's
settings (Actions, General, Workflow permissions).

### Pin digests by hand

`fspec update --images` is what the workflow's second job runs. You can run it yourself after an
image is pushed:

```console
$ fspec update --images
+ image desktop ghcr.io/alex/fspec-desktop@sha256:6f1c...
```

It writes one table per image host:

```toml
[system.desktop]
image = "ghcr.io/alex/fspec-desktop@sha256:6f1c..."
```

Only ghcr.io registries are supported.

### What apply does

On an image host, apply reads `bootc status --json`. When the staged image, or the booted one if
none is staged, has a different digest than the lock, apply runs:

```sh
bootc switch ghcr.io/alex/fspec-desktop@sha256:6f1c...
```

It records `bootc rollback` as the undo, so a failed apply puts the old deployment back. Nothing
changes on the running system until you reboot. `fspec plan` reports drift as
`running <digest>, lock wants <digest>` when the machine is on another image.

To go back to the previous image, pick it in the boot menu, or run `bootc rollback` and reboot.

Packages from FormalSpec's store, Flatpaks, files and user services sit outside the image. They
are applied the same way on an image host as on any other host.

## Classic distros

On a classic distro, fspec runs the distro's package manager on the machine. The `packages` list
holds the base packages, named the way the distro names them:

```starlark
system = fedora(packages = ["NetworkManager", "sudo", "btrfs-progs", "gdm", "gnome-shell"]),
```

| Builtin | Installs with |
| --- | --- |
| `fedora()` | `dnf` |
| `arch()` | `pacman -S --needed` |
| `debian()`, `ubuntu()` | `apt-get` |
| `void()` | `xbps-install` |
| `tumbleweed()` | `zypper --non-interactive install --auto-agree-with-licenses` |
| `aerynos()` | `moss install` |

Remove a name from the list and the next apply removes the package. Remove the whole
`fedora(...)` block and its packages stay installed.

Base packages are not in the lock. They install at whatever version the distro's repositories
hold, and fspec does not cache them. To pin a package that matters, declare it as a `pkg` with the
distro's source as well:

```starlark
pkg("mesa-dri-drivers", source = dnf())
```

`fspec update` records that package's exact version, URL and hash. Apply downloads the file into
`/var/cache/fspec/pkgs` and installs it from there (`dnf install`, `pacman -U`, `apt-get install`,
or a local xbps repository). A rollback reinstalls the previous cached file. On Void, xbps has no
exact-version install, so it can still prefer a newer version from the Void repositories.

Base packages install before services, so a `service("sshd")` can enable a unit that the same
apply installed. [Packages](packages.md) covers `pkg` sources in full.

### btrfs snapshots before system changes

When the root filesystem is btrfs, every apply that changes the system block or a `repo` first
takes a read-only snapshot of `/`:

```text
/.snapshots/fspec-<generation>
```

The snapshot is taken before any package changes commit, so it holds the old kernel and the old
packages. A failed apply leaves it in place. fspec never deletes these snapshots; remove old ones
with `sudo btrfs subvolume delete /.snapshots/fspec-<generation>`. Other filesystems get no
snapshot.

## openSUSE Tumbleweed

Tumbleweed is a rolling distro, so a package name alone pins nothing. FormalSpec pins the whole
distro instead: openSUSE publishes a dated snapshot of the repositories every few days and keeps
about a month of them, and the lock names one.

```starlark
host(
    name = "laptop",
    platform = "x86_64-linux",
    system = tumbleweed(packages = ["NetworkManager", "sudo", "tuned-ppd", "!power-profiles-daemon"]),
    ...
)
```

1. Pin a snapshot:

   ```console
   $ fspec update
   + snapshot laptop 20261020
   ```

   The lock gets:

   ```toml
   [zypper.laptop]
   snapshot = "20261020"
   ```

   `fspec update` picks the newest snapshot. With `release_age` on the host, it picks the newest
   one at least that old.

2. Apply. fspec points the distro at that snapshot:

   - writes the date to the zypp variable `/etc/zypp/vars.d/fspec_snapshot`;
   - writes `/etc/zypp/repos.d/fspec-oss.repo` and `fspec-non-oss.repo`, pointing at
     `https://download.opensuse.org/history/${fspec_snapshot}/tumbleweed/repo/oss/` (and `non-oss`);
   - disables the rolling repositories (`repo-oss`, `repo-non-oss`, `repo-update`, and the debug
     and source ones when enabled);
   - runs `zypper --non-interactive dist-upgrade --auto-agree-with-licenses`.

Moving the pin with the next `fspec update` rewrites the variable and runs the upgrade again.
Moving it back to an older date downgrades. A failed apply restores the old variable and
repository files and upgrades against them, so the machine returns to the snapshot it started
from. Other repositories you added stay as they are.

A `"!name"` entry in `packages` removes that package in the same transaction, for one that
conflicts with a package you want (`"tuned-ppd", "!power-profiles-daemon"`). A rollback removes
only the packages that apply installed, never one that was there before, since zypper would also
remove everything that depends on it.

`tumbleweed(kernel_args = [...])` keeps kernel options in `/etc/kernel/cmdline` and the boot
entries. It works only on a root that `fspec switch --beside` installed
([Switching distros](switching-distros.md)).

### Third-party zypper repositories

Add a vendor's repository with `repo(...)` in the host's modules:

```starlark
repo(
    "1password",
    url = "https://downloads.1password.com/linux/rpm/stable/x86_64",
    key = "https://downloads.1password.com/linux/keys/1password.asc",
    fingerprint = "3FEF9748469ADBE15DA7CA80AC2D62742012EA22",
),
pkg("1password", source = zypper(repo = "1password")),
```

`key` and `fingerprint` go together. Before anything changes, apply downloads the key to
`/var/cache/fspec/keys/<name>.asc` and checks its fingerprint with `gpg`. A different fingerprint
stops the apply. Then apply imports the key with `rpm --import`, unless rpm already has it, and
runs `zypper addrepo --refresh <url> <name>`. Removing the `repo` runs `zypper removerepo` and
keeps the key.

Repositories are added before `pkg` packages install, so a package from the repository installs in
the same apply.

A repository the host cannot reach does not fail the apply. When the key cannot be downloaded, or
`zypper refresh` fails after the add (the vendor is down, or a filtering proxy intercepts its
TLS), apply prints a warning and leaves the repository disabled, so the distro upgrade still
works:

```text
fspec: repo:1password: fetching its key failed, deferring the repo: ...
fspec: deferred repo:1password: its key https://downloads.1password.com/linux/keys/1password.asc could not be fetched; the next apply tries it again
```

A `pkg` tied to it with `zypper(repo = "1password")` waits with it, and so do the services,
timers and files that package owns. The next apply tries again.

## AerynOS

On AerynOS, `aerynos(packages = [...])` and `pkg(..., source = moss())` run `moss install` and
`moss remove`. moss builds each transaction as a new `/usr` and swaps it in at once, and numbers
each result as a state.

Before each moss change, fspec records `moss state activate <the current state>` as the undo. A
failed apply returns the system to the exact state it started from, including any upgrades that
came with it. moss packages lock by name only: they install from AerynOS's repository stream.

The boot menu lists the newest five moss states, so you can also boot an earlier state from
there.

## Troubleshooting

**`fspec.lock has no image for host desktop; run `fspec ci init`, let the workflow push the image, then `fspec update --images``**

The host has no digest in the lock yet. Follow [Set up an image host](#set-up-an-image-host).

**`resolving ghcr.io/alex/fspec-desktop:latest (the ghcr package must be public for anonymous pulls)`**

The workflow has not pushed the image yet, or the package is private. Make it public on GitHub.

**`cannot read the git remote `origin` in <dir>; pass --registry ghcr.io/<owner>`**

`fspec ci init` and `fspec update --images` read the owner from `origin`. Add the remote, or pass
`--registry`.

**`no host declares an image(...), so there is nothing to build`**

`fspec ci init` only writes a workflow when at least one host has `system = image(...)`.

**`image packages use dnf(...) but the base `<base>` is a debian image, which has no dnf`**

`image(packages = ...)` layers packages with dnf. Use a dnf-based base image, or install the
packages in `steps`.

**`fspec.lock pins no Tumbleweed snapshot for host laptop; run `fspec update``**

Run `fspec update` and commit the lock.

**`repo:nvidia: <url> has fingerprint <found>, the spec pins <expected>`**

The vendor's key is not the one you pinned. Check the vendor's published fingerprint before you
change `fingerprint`.

**`repo:1password: key and fingerprint go together`**

Set both, or neither.

**`repo:<name>: repo(...) is for zypper distros, fedora is not one`**

`repo(...)` works on Tumbleweed only. On other distros, add repositories with `file(...)` or, on
image hosts, `image(repos = [...])`.

**`kernel_args need a root that `fspec switch --beside` installed; this one has no /etc/systemd/system/fspec-esp-sync.service`**

`tumbleweed(kernel_args = ...)` writes boot entries through the sync service that
`fspec switch --beside` installs. On another Tumbleweed install, set kernel options with the
distro's own tools.
