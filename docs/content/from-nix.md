+++
title = "Coming from Nix"
description = "How NixOS, nix-darwin and Home Manager constructs map to FormalSpec, with side-by-side examples and what you give up."
+++

FormalSpec keeps the parts of Nix you would miss: a programmable config, one lock for every
host, generations and rollback, and clean removal. It drops nixpkgs, the store-wide build graph
and hand-written hashes. This page shows the five moves you make first, side by side, then maps
the rest of a NixOS, nix-darwin and Home Manager setup construct by construct.

A spec is Starlark, a small dialect of Python, instead of the Nix language. A module is a plain
function that returns resources, and a condition is a plain `if`. There is no module system to
merge options, no fixpoint and no laziness to reason about. [Hosts and
modules](concepts/hosts-and-modules.md) covers the structure.

## The five common moves

### home.packages to pkg

```nix
home.packages = with pkgs; [ ripgrep fd jq nodePackages.prettier ];
```

```starlark
modules = [
    pkg("ripgrep"),
    pkg("fd", source = github("sharkdp/fd")),
    pkg("jq"),
    pkg("prettier", source = npm("prettier")),
]
```

`pkg("ripgrep")` takes the upstream release from the aqua registry. Other sources name where
upstream publishes: GitHub releases, npm, crates.io, Flathub, Homebrew or the distro. Then lock
the versions:

```sh
fspec update
```

There is no channel to bump. `fspec update` asks each source for its newest release and writes
the version and hash into `fspec.lock`. [Packages](guides/packages.md) covers every source.

### home.file to file

```nix
home.file.".config/git/ignore".text = ".DS_Store\n";
xdg.configFile."kitty/kitty.conf".source = ./kitty.conf;
```

```starlark
file("~/.config/git/ignore", content = ".DS_Store\n"),
file("~/.config/kitty/kitty.conf", src = "kitty.conf"),
```

The default `managed` mode writes a real file, not a symlink into a read-only store, so an app
that rewrites its config does not fail. fspec reports the edit as drift and puts its content back
on the next apply. To link instead, the way `mkOutOfStoreSymlink` does, set `mode = "link"`.
[Files](guides/files.md) covers `seed` and `patch` too.

### systemd.user.services to service

```nix
systemd.user.services.syncthing = {
  Unit.Description = "Syncthing";
  Service = {
    ExecStart = "${pkgs.syncthing}/bin/syncthing serve --no-browser";
    Restart = "on-failure";
  };
  Install.WantedBy = [ "default.target" ];
};
```

```starlark
service("syncthing",
        exec = "/opt/fspec/current/bin/syncthing serve --no-browser",
        user = True,
        restart = "on_failure",
        description = "Syncthing")
```

One `service(...)` renders for the host's init system: a systemd unit, a launchd agent on macOS,
or a runit, OpenRC, dinit or s6 service. `/opt/fspec/current/bin` is the stable path of every
store package, the counterpart of `${pkgs.syncthing}`. Put the service in
`pkg("syncthing", services = [...])` and removing the package removes the service with it.
[Services](guides/services.md) covers timers and init-specific settings.

### A flake devShell to dev.star

```nix
devShells.default = pkgs.mkShell {
  packages = [ pkgs.just pkgs.bun pkgs.cmake pkgs.pkg-config pkgs.openssl ];
  RUST_LOG = "info";
  shellHook = "export DATABASE_URL=postgres://localhost/app";
};
```

```starlark
dev(
    packages = [pkg("just"), pkg("bun")],
    libs = [lib("openssl")],
    bins = [bin("cmake")],
    env = {"RUST_LOG": "info"},
    shell_hook = "export DATABASE_URL=postgres://localhost/app",
)
```

Save it as `dev.star` at the project root, then lock and enter it:

```sh
fspec update
fspec dev
```

Tools come from FormalSpec's own sources and are pinned in the project's `fspec.lock`. Native
libraries and build tools such as `openssl` and `cmake` come from the machine's package manager:
`fspec update` records which package provides each one on each package manager. `use fspec`
in an `.envrc` is the counterpart of `use flake`.
[Project environments](guides/dev-environments.md) covers services and containers.

### nixos-rebuild switch --target-host to fspec apply --host

```sh
nixos-rebuild switch --flake .#server --target-host alex@server --use-remote-sudo
```

```starlark
host(name = "server", platform = "x86_64-linux", system = debian(), ssh = "alex@server",
     modules = [...])
```

```sh
fspec apply --host server
```

fspec ships the committed spec and the lock to the host over ssh and applies there, with that
host's own fspec. It refuses a spec tree with uncommitted changes unless you pass `--dirty`, so
every host applies a commit you can name. `fspec apply --all` applies every host, remote ones
first. [Remote hosts](guides/remote-hosts.md) covers builders and `plan --all`.

## What you gain

- New releases the day upstream ships them. You do not wait for a nixpkgs pull request or add a
  flake input for each fast-moving tool.
- No hashes to write. `fspec update` computes them, and the lock is still exact.
- Apps that update themselves or rewrite their own config keep working, since nothing is a
  read-only store symlink unless you ask for `link`.
- Any distro, without owning the OS. Binaries built for an ordinary Linux run without nix-ld or
  FHS wrappers.
- Distros on Epic's signed-kernel list (Fedora, Universal Blue and others), so games that use
  EAC on Linux can run.
- No compiling on weak machines. Prebuilt artifacts are the default, and CI builds the patched
  ones.
- A smaller model: resources and a state file. There are no derivations, no module fixpoint and
  no import from derivation.

## What you give up

- Runtime dependencies are not pinned down to glibc. Prebuilt dynamic binaries use the base
  OS's libraries. Pinning the base too (a bootc image digest in the lock) and preferring static
  builds and Flatpaks closes most of that gap.
- You maintain every patched or custom build yourself. nixpkgs has about 100,000 recipes.
  FormalSpec reuses existing registries (aqua, Homebrew, Flathub, npm) and has no recipes of its
  own. The `nix(...)` source borrows a nixpkgs build on hosts that have Nix, where that is
  cheaper.
- An atomic rollback of the kernel and bootloader needs a bootc image host. On a classic distro,
  `fspec rollback` restores packages, files and services, not the boot chain.
- macOS itself cannot be pinned, the same as with nix-darwin.
- FormalSpec is early. Packages, files, services, removal and rollback run on real machines.
  bootc images and `fspec switch`, the parts a full NixOS replacement needs, have only run
  against test roots so far.

## Mapping a NixOS and nix-darwin setup

| Nix | FormalSpec |
| --- | --- |
| `flake.nix` inputs, `flake.lock` | `fspec.lock` |
| `nixosConfigurations.<host>` | `host(...)` in `spec.star` |
| mixins and profiles | Modules: functions that return resources |
| `lib.mkIf cfg.enable` | A plain `if` in Starlark |
| `home.packages`, `environment.systemPackages` | `pkg(...)` |
| `home.file`, `xdg.configFile` | `file(...)`, `managed` mode |
| `mkOutOfStoreSymlink` | `file(..., mode = "link")` |
| Seed-once activation scripts | `file(..., mode = "seed")` |
| A JSON file you rewrite with `jq` on activation | `file(..., mode = "patch")` |
| `overrideAttrs` with `patches` | `pkg(..., patches = [...], build_flags = [...])` |
| `systemd.services`, `systemd.user.services`, `launchd.agents` | `service(...)` |
| `homebrew.brews`, `homebrew.casks`, `homebrew.taps` | `pkg(..., source = brew(..., tap = "owner/repo"))` |
| `home.packages` apps under `~/Applications/Home Manager Apps`, mac-app-util | `pkg(..., apps = ["Foo.app"])`, copied into `/Applications` |
| `system.defaults` | `defaults(...)`, with `system = True` for `/Library/Preferences` |
| `power.sleep`, `system.activationScripts` that run pmset, systemsetup or socketfilterfw | `setting("pmset", "sleep", 0)` and the other `setting(...)` tools |
| agenix | `secret(...)` |
| `boot.kernelPackages`, `boot.kernelParams`, the NVIDIA module | `image(...)` with `kernel_args`, or `tumbleweed(kernel_args = [...])` |
| `services.flatpak` | `pkg(..., source = flatpak())` |
| `nixos-rebuild switch --target-host` | `fspec apply --host` |
| `nix.buildMachines`, `max-jobs = 0` | `host(builders = [builder("<host>", routes = [...])])` |
| `environment.systemPackages` that the distro ships | The distro's package list, such as `tumbleweed(packages = [...])`, before any `git(...)` build |
| `fileSystems.<path>` | A `.mount` (and `.automount`) unit as a `file(...)` plus a `service(...)` |
| `programs.ssh.knownHosts` | `file("/etc/ssh/ssh_known_hosts", ...)` |
| `networking.firewall` | firewalld zone files in `/etc/firewalld/zones` |
| `nix.gc.automatic` | `timer(...)` that runs `fspec gc --older-than` |
| A Home Manager `if host == ...` | A `<host>/home/` tree laid over the shared `home/` |
| `nix-collect-garbage --delete-older-than` | `fspec gc --older-than` |
| `programs.neovim.plugins` | `vim.pack.add` in `init.lua`, with `nvim-pack-lock.json` as a `managed` file next to it |
| `nvim-treesitter.withAllGrammars` | nvim-treesitter's own `install()`, which compiles parsers with `tree-sitter-cli` |
| `launchd.daemons`, `launchd.user.agents` with `StartCalendarInterval` | `timer(...)` |
| A login item (`open -g -j -a App.app` at load) | `service(..., user = True)` with the default `restart = "no"` |
| `users.users.<u>.openssh.authorizedKeys` | A `file(...)` in `/etc/ssh/authorized_keys.d/<u>`, named by `AuthorizedKeysFile`. fspec writes real files, so nix-darwin's `AuthorizedKeysCommand /bin/cat` is not needed |
| `security.sudo.extraConfig` | `file("/etc/sudoers.d/...", perms = "0440")` |
| `fonts.packages` with vendored files | `file("/Library/Fonts/<font>", src = ...)` on macOS |
| `launchd` `serviceConfig` keys (`StartInterval`, `WatchPaths`, `ThrottleInterval`, `StandardOutPath`, `ProcessType`) | `service(..., launchd = {...})` |
| `mkOutOfStoreSymlink` to a directory or a path outside the repository | `file(..., mode = "link", target = "...")` |
| `homebrew.taps` | `brew(tap = "owner/repo")` on the `pkg` that needs it |
| `${pkg}/lib/...` named by absolute path (a PAM or Lua module) | `/opt/fspec/current/pkgs/<name>/lib/...` |
| `power.sleep.computer`, `power.restartAfterFreeze`, `networking.applicationFirewall`, `pmset` or `systemsetup` in activation scripts | `setting("pmset" \| "systemsetup" \| "firewall", key, value)` |
| `system.defaults` for a root domain, or `defaults write /Library/Preferences/...` in activation | `defaults(..., system = True)` |
| Extra `fetchurl` sources unpacked into `$out` in `installPhase` (Discord's modules) | `pkg(..., parts = {"<dir>": url(...)})` |
| A Syncthing folder that carries an app's profile or tokens between machines | `sync(...)`, with the keyring item that unlocks it. See [Sync between hosts](guides/sync.md) |
| `$out/Applications/Foo.app` with mac-app-util or `copyApps` | `pkg(..., apps = ["Foo.app"])`, from a release `.dmg` asset where upstream ships one |

## A three-host example

Take a Mac dev machine on nix-darwin and two Hyprland laptops on NixOS, one of them with an
NVIDIA GPU.

The Mac ports most directly. Homebrew stays Homebrew, launchd agents become `service(...)`, and
`system.defaults`, Dock pins and login items become `defaults(...)` and files. A PAM module built
from source for macOS becomes a `git(...)` package you maintain.

The NVIDIA laptop moves to openSUSE Tumbleweed with NVIDIA's openSUSE repository. Its GPU
switching service, asusd and tuned become files and `service(...)` units. Packages come from
zypper first, and from `git(...)` only where no repository or release has them. Hyprland's config
and its theme templates are `managed` files with the same content as today. A desktop shell and a
Bluetooth daemon with local patches become `git(...)` packages that CI builds. Steam and the
other game launchers become Flatpaks in `self_managed` mode.

The other laptop uses the same modules without NVIDIA, and builds its `git(...)` packages on the
NVIDIA laptop through `builders`.

All three apply the same commit. `fspec plan --all` shows drift on every host at once.
