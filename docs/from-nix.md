# Coming from Nix

FormalSpec keeps the parts of Nix you'd miss: a programmable config, one lock
for every host, generations and rollback, and clean removal. It drops nixpkgs,
the store-wide build graph and hand-written hashes.

## What you gain

- New releases the day upstream ships them. You don't wait on a nixpkgs PR or
  add a flake input for each fast-moving tool.
- No hashes to write. `fspec update` computes them, and the lock is still
  exact.
- Apps that update themselves or rewrite their own config keep working,
  because nothing is a read-only store symlink unless you ask for `link`.
- Any distro, and you don't have to own the OS. Binaries built for normal
  Linux run without nix-ld or FHS wrappers.
- Distros on Epic's signed-kernel list (Fedora, Universal Blue and others), so
  games with EAC Linux v2 can run.
- No compiling on weak machines. Prebuilt artifacts are the default, and CI
  builds the patched ones.
- A smaller mental model: resources and a state file. There are no
  derivations, no module fixpoint and no IFD.

## What you give up

- Runtime dependencies aren't pinned down to glibc. Prebuilt dynamic binaries
  use the base OS's libraries. Pin the base too (a bootc image digest in the
  lock) and prefer static builds and Flatpaks to close most of that gap.
- You maintain every patched or custom build yourself. nixpkgs gives you about
  100,000 recipes; FormalSpec reuses existing registries (aqua, Homebrew,
  Flathub, npm) but has no recipes of its own. The `nix(...)` source can borrow
  a nixpkgs build where that's cheaper.
- The kernel and bootloader need bootc for atomic rollback. Classic distros
  fall back to btrfs snapshots.
- macOS can't be pinned, same as with nix-darwin.
- It doesn't exist yet. Steps 1 to 4 of the build order replace home-manager
  on one machine; matching a full NixOS setup takes the rest.

## Mapping a NixOS and nix-darwin setup

| Nix | FormalSpec |
| --- | --- |
| `flake.nix` inputs, `flake.lock` | `fspec.lock` |
| `nixosConfigurations.<host>` | `host(...)` in `spec.star` |
| mixins and profiles | modules: functions returning resources |
| `lib.mkIf cfg.enable` | a plain `if` in Starlark |
| `home.packages`, `environment.systemPackages` | `pkg(...)` |
| `home.file`, `xdg.configFile` | `file(...)`, `managed` mode |
| `mkOutOfStoreSymlink` | `file(..., mode = "link")` |
| seed-once activation scripts | `file(..., mode = "seed")` |
| a JSON file you rewrite with `jq` on activation | `file(..., mode = "patch")` |
| `overrideAttrs` with `patches` | `pkg(..., patches = [...], build_flags = [...])` |
| `systemd.services`, `systemd.user.services`, `launchd.agents` | `service(...)` |
| `homebrew.brews`, `homebrew.casks`, `homebrew.taps` | `pkg(..., source = brew(..., tap = "owner/repo"))` |
| `home.packages` apps under `~/Applications/Home Manager Apps`, mac-app-util | `pkg(..., apps = ["Foo.app"])`, copied into /Applications |
| `system.defaults` | `defaults(...)`, `system = True` for /Library/Preferences |
| `power.sleep`, `system.activationScripts` running pmset, systemsetup or socketfilterfw | `setting("pmset", "sleep", 0)` and friends |
| agenix | `secret(...)` |
| `boot.kernelPackages`, `boot.kernelParams`, NVIDIA module | `image(...)` with `kernel_args`, or `tumbleweed(kernel_args = [...])` |
| `services.flatpak` | `pkg(..., source = flatpak())` |
| `nixos-rebuild switch --target-host` | `fspec apply --host` |
| `nix.buildMachines`, `max-jobs = 0` | `host(builders = [builder("<host>", routes = [...])])` |
| `environment.systemPackages` that the distro ships | the distro's package list on `tumbleweed(...)`, before any `git(...)` build |
| `fileSystems.<path>` | a `.mount` (and `.automount`) unit as a `file(...)` plus `service(...)` |
| `programs.ssh.knownHosts` | `file("/etc/ssh/ssh_known_hosts", ...)` |
| `networking.firewall` | firewalld zone files in `/etc/firewalld/zones` |
| `i18n.defaultLocale`, `console.keyMap`, `time.timeZone` | `host(locale = ..., keymap = ..., timezone = ...)` |
| `nix.gc.automatic` | `timer(...)` running `fspec gc --older-than` |
| a Home Manager `if host == ...` | a `<host>/home/` tree overlaid on the shared `home/` |
| `nix-collect-garbage --delete-older-than` | `fspec gc --older-than` |
| `programs.neovim.plugins` | `vim.pack.add` in `init.lua`, with `nvim-pack-lock.json` as a `managed` file next to it |
| `nvim-treesitter.withAllGrammars` | nvim-treesitter's own `install()`, which compiles parsers with `tree-sitter-cli` |
| `launchd.daemons`, `launchd.user.agents` with `StartCalendarInterval` | `timer(...)` |
| a login item (`open -g -j -a App.app` at load) | `service(..., user = True)` with the default `restart = "no"` |
| `users.users.<u>.openssh.authorizedKeys` | a `file(...)` in `/etc/ssh/authorized_keys.d/<u>` named by `AuthorizedKeysFile`; fspec writes real files, so nix-darwin's `AuthorizedKeysCommand /bin/cat` is not needed |
| `security.sudo.extraConfig` | `file("/etc/sudoers.d/...", perms = "0440")` |
| `fonts.packages` with vendored files | `file("/Library/Fonts/<font>", src = ...)` on macOS |
| `launchd` `serviceConfig` keys (`StartInterval`, `WatchPaths`, `ThrottleInterval`, `StandardOutPath`, `ProcessType`) | `service(..., launchd = {...})` |
| `mkOutOfStoreSymlink` to a directory or a path outside the repo | `file(..., mode = "link", target = "...")` |
| `homebrew.taps` | `brew(tap = "owner/repo")` on the pkg that needs it |
| `${pkg}/lib/...` named by absolute path (a PAM or Lua module) | `/opt/fspec/current/pkgs/<name>/lib/...` |
| `power.sleep.computer`, `power.restartAfterFreeze`, `networking.applicationFirewall`, `pmset`/`systemsetup` in activation scripts | `setting("pmset" \| "systemsetup" \| "firewall", key, value)` |
| `system.defaults` for a root domain, or `defaults write /Library/Preferences/...` in activation | `defaults(..., system = True)` |
| Extra `fetchurl` sources unpacked into `$out` in `installPhase` (Discord's modules) | `pkg(..., parts = {"<dir>": url(...)})` |
| A syncthing folder that carries an app's profile or tokens between machines | `sync(...)`, with the keyring item that unlocks it |
| `$out/Applications/Foo.app` with mac-app-util or `copyApps` | `pkg(..., apps = ["Foo.app"])`, from a release `.dmg` asset where upstream ships one |

### A three-host example

Take a macbook dev server on nix-darwin plus two Hyprland laptops on NixOS,
one with an NVIDIA dGPU.

The macbook ports most directly. Homebrew stays Homebrew, launchd agents become
`service(...)`, and `system.defaults`, dock pins and login items become
`defaults(...)` and files. A PAM module built from source for macOS becomes a
`git(...)` package you maintain.

The NVIDIA laptop moves to Tumbleweed with NVIDIA's openSUSE repo. The MUX
service, asusd and tuned become files and `service(...)` units; packages come
from zypper first and `git(...)` only where no repo or release has them. Hyprland's Lua config and
matugen templates are `managed` files with the same content as today. A desktop
shell and a Bluetooth daemon with local patches become `git(...)` packages that
CI builds. Steam and the other game launchers become Flatpaks in
`self_managed` mode.

The Intel laptop uses the same modules without NVIDIA, and builds its
`git(...)` packages on the NVIDIA laptop through `builders`.

All three apply the same commit. `fspec plan --all` shows drift on every host
at once.
