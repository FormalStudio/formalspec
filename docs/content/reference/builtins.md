+++
title = "Builtins"
description = "Every function a spec.star, dev.star or secrets.star can call, with its arguments and defaults."
+++

A spec is Starlark: Python syntax with `def`, `if`, `for`, comprehensions and `lambda`, and no
access to files, the network or the clock. On top of the language, FormalSpec adds the builtins
on this page.

Every builtin checks its arguments when it runs, so a misspelled or unknown field fails on the
line that wrote it. An argument passed as `None` counts as unset. Positional arguments are
listed in the signature; everything else is a keyword.

Besides these, `spec.star` and `dev.star` have Starlark's `struct`, `json.encode`,
`json.decode` and `print`.

## Files and their globals

| File | Must define | Has |
| --- | --- | --- |
| `spec.star` | `hosts = [host(...), ...]` | every builtin on this page except `dev`, `lib` and `bin`; `load("//...")` and `load("gh:...")` |
| `dev.star` | one call to `dev(...)` | `dev`, `lib`, `bin`, `pkg`, `pypi`, the source builtins, `service` (the project form), a `platform` global; `load("//...")` |
| `secrets.star` | `secrets = {...}` | `struct` and `print` only, no `load` |

`load("//modules/cli.star", "cli")` resolves from the directory that holds `spec.star`. Relative
loads (`load("cli.star")`) are an error. [Hosts and modules](../concepts/hosts-and-modules.md)
covers `gh:` loads.

## host

```starlark
host(name, platform, system = None, users = [], modules = [], ssh = name,
     protect = [], persist = [], release_age = None, builders = [], sync = [],
     locale = None, keymap = None, timezone = None)
```

| Argument | Type | Default | Meaning |
| --- | --- | --- | --- |
| `name` | string | required | The host's name. A host is this machine when the name equals the system host name, lowercased, up to the first dot |
| `platform` | string | required | `x86_64-linux`, `aarch64-linux`, `x86_64-darwin` or `aarch64-darwin` |
| `system` | `image(...)` or a distro builtin | none | The system layer. Required on Linux, not allowed on macOS |
| `users` | list of `user(...)` | `[]` | Accounts this host manages |
| `modules` | resource, list, function or `None` | `[]` | Everything else the host has. Lists nest; a function is called with the host's facts |
| `ssh` | string | the name | The ssh destination `fspec apply` reaches the host by |
| `protect` | list of paths | `[]` | Paths FormalSpec refuses to act under, such as `/home`. The applying user's own home stays usable |
| `persist` | list of paths | `[]` | Paths `fspec switch --reset-var` keeps |
| `release_age` | string | none | Resolve packages to the newest release at least this old: `3d`, `12h`, `2w` (units `m`, `h`, `d`, `w`) |
| `builders` | list of `builder(...)` | `[]` | Machines that build `git(...)` packages for this host |
| `sync` | list of `sync(...)` | `[]` | App data this host syncs with its peers |
| `locale` | string | none | `LANG` for the system, with a character set: `en_US.UTF-8`. fspec generates it where the distro needs that. Linux only |
| `keymap` | string | none | Keyboard layout for the console and the desktop: `us`, `de`. Linux with systemd, or Void |
| `timezone` | string | none | A zoneinfo name: `Europe/Madrid`. `/etc/localtime` links to it. Linux only |

A function in `modules` receives a struct with:

| Field | Example |
| --- | --- |
| `name` | `"laptop"` |
| `platform.name` | `"aarch64-darwin"` |
| `platform.os` | `"linux"` or `"darwin"` |
| `platform.arch` | `"x86_64"` or `"aarch64"` |
| `platform.is_linux`, `platform.is_darwin` | `True` or `False` |
| `distro` | `"fedora"`, `"bazzite"`, `"arch"`, `"debian"`, `"ubuntu"`, `"void"`, `"aerynos"`, `"opensuse-tumbleweed"`, `"macos"` |
| `init` | `"systemd"`, `"launchd"`, `"runit"`, `"openrc"`, `"dinit"`, `"s6"` |

`modules` takes `pkg`, `file`, `service`, `timer`, `user`, `secret`, `defaults`, `setting`, `repo`
and `sync`. A `sync` in `modules` joins the host's `sync` list. `image(...)` and the distro
builtins go in `system`, never in `modules`.

## System layer

### image

```starlark
image(base, packages = None, copr = [], repos = [], stages = [], steps = [],
      kernel_args = [], distro = None)
```

A bootc image, built by CI and switched to with `bootc switch`. See
[The system layer](../guides/system-layer.md).

| Argument | Type | Default | Meaning |
| --- | --- | --- | --- |
| `base` | string | required | Base image, such as `ghcr.io/ublue-os/bazzite-nvidia:stable` |
| `packages` | `dnf(...)` | none | Packages layered into the image. The base must be a dnf distro |
| `copr` | list of strings | `[]` | COPR projects (`owner/project`) enabled before the packages install |
| `repos` | list of URLs | `[]` | `.repo` files added before the packages install |
| `stages` | list of strings | `[]` | Containerfile stages placed before the image's `FROM`; each starts with `FROM` |
| `steps` | list of strings | `[]` | Containerfile instructions run after the packages |
| `kernel_args` | list of strings | `[]` | Kernel arguments, written to `/usr/lib/bootc/kargs.d/fspec.toml` |
| `distro` | string | from the base name | The distro the image is; `bazzite-nvidia` gives `bazzite` |

An image host runs systemd.

### fedora, arch, debian, ubuntu, void, aerynos, tumbleweed

```starlark
fedora(packages = [], init = "systemd")
tumbleweed(packages = [], kernel_args = [])
```

A classic distro. All seven take keywords only.

| Builtin | Distro | Package manager | Default init |
| --- | --- | --- | --- |
| `fedora()` | Fedora Workstation and spins | dnf | systemd |
| `arch()` | Arch Linux | pacman | systemd |
| `debian()` | Debian | apt | systemd |
| `ubuntu()` | Ubuntu | apt | systemd |
| `void()` | Void Linux | xbps | runit |
| `aerynos()` | AerynOS | moss | systemd |
| `tumbleweed()` | openSUSE Tumbleweed | zypper | systemd |

| Argument | Type | Default | Meaning |
| --- | --- | --- | --- |
| `packages` | list of strings | `[]` | Base packages, named the way the distro names them. Removing a name removes the package. On Tumbleweed, `"!name"` removes a conflicting package in the same transaction |
| `init` | string | per distro | `systemd`, `runit`, `openrc`, `dinit` or `s6` |
| `kernel_args` | list of strings | `[]` | Tumbleweed only: options kept in `/etc/kernel/cmdline`, one option each, no spaces |

### repo

```starlark
repo(name, url, key = None, fingerprint = None)
```

A third-party zypper repository. Tumbleweed only.

| Argument | Type | Default | Meaning |
| --- | --- | --- | --- |
| `name` | string | required | The repository alias |
| `url` | string | required | The repository URL |
| `key` | URL | none | Signing key, imported with `rpm --import` |
| `fingerprint` | string | none | The key's full 40-digit fingerprint. Required with `key` |

## pkg

```starlark
pkg(name, source = aqua(name), version = None, mode = "pinned", channel = "stable", ...)
```

A package. See [Packages](../guides/packages.md).

| Argument | Type | Default | Meaning |
| --- | --- | --- | --- |
| `name` | string | required | The package name, unique per host |
| `source` | a source builtin | `aqua(name)` | Where it comes from. `pkg("python")` defaults to CPython from python-build-standalone |
| `version` | string | none | Lock this exact upstream version instead of the newest |
| `mode` | string | `"pinned"` | `"pinned"`: the lock's version, always. `"self_managed"`: install once, then leave updates to the app |
| `channel` | string | `"stable"` | `"bleeding"` ignores the host's `release_age` |
| `bins` | list of strings | `[]` | Only link these executables, by file name. Empty links every command the package ships |
| `links` | dict | `{}` | Extra command names for one of its executables: `{"cargo": "rustup"}` |
| `share` | dict | `{}` | Linux: link a path under `/usr/local/share` to a path inside the package (`"."` is its root). For fonts and themes |
| `etc` | dict | `{}` | Linux: link a path under `/etc` to a path inside the package |
| `apps` | list of strings | `[]` | macOS: `.app` bundles inside the package, copied to `/Applications` |
| `parts` | dict | `{}` | Extra `url(...)` archives unpacked into the package, keyed by the path inside it |
| `patches` | list of paths | `[]` | Patch files in the spec repository, applied in order |
| `build` | list of strings | `[]` | For `git(...)`: the build command, replacing recipe detection |
| `build_flags` | list of strings | `[]` | For `git(...)`: appended to the recipe's configure or build command |
| `files` | list of `file(...)` | `[]` | Files that belong to the package and go with it |
| `services` | list of `service(...)` | `[]` | Services that belong to the package and go with it |
| `timers` | list of `timer(...)` | `[]` | Timers that belong to the package and go with it |
| `data` | list of paths | `[]` | App data moved to the trash when the package is removed |
| `keep_data` | bool | `False` | Leave app data in place on removal |

### pypi

```starlark
pypi(name, ...)
```

A PyPI distribution, for `dev.star`. Same arguments as `pkg` except `source`.

## Sources

Each returns a value for `pkg(source = ...)`.

| Builtin | Positional | Keywords | Installs from |
| --- | --- | --- | --- |
| `aqua(name)` | `name` | | The aqua registry, which maps tool names to their GitHub releases |
| `github(repo)` | `owner/name` | `asset`: a glob over release asset names | GitHub releases, the asset matching the platform |
| `npm(name)` | `name` | | The npm registry |
| `cargo(name)` | `name` | | A crate's prebuilt GitHub release assets. Never compiles |
| `flatpak(id)` | `id` | `remote = "flathub"` | Flathub or another remote. Linux |
| `brew(name)` | `name` | `cask = False`, `tap = "owner/repo"` | Homebrew formulae and casks. macOS |
| `url(url)` | `url` | | One file or archive at a fixed URL |
| `git(url, rev = "main")` | `url`, `rev` | `deps = []`, `image = None` | A source build at a commit. See [Building from source](../guides/source-builds.md) |
| `nix(attr)` | `attr` | | A nixpkgs attribute, on hosts that have Nix |
| `dnf(names)`, `pacman(names)`, `apt(names)`, `xbps(names)`, `moss(names)`, `zypper(names)` | a name or a list | `repo` (zypper only) | The distro's repositories, on hosts of that distro |

`names` defaults to the package's own name. `zypper("nordvpn", repo = "nordvpn")` ties the
package to a `repo(...)`, so it waits with that repository when the repository is unreachable.

## file

```starlark
file(path, src = None, content = None, target = None, mode = "managed",
     format = None, owns = {}, on_conflict = "ours", on_drift = "overwrite", perms = None)
```

A file. A path under `~/` is in the user scope; an absolute path is in the system scope. See
[Files](../guides/files.md).

| Argument | Type | Default | Meaning |
| --- | --- | --- | --- |
| `path` | string | required | Absolute, or `~/...` |
| `src` | path | none | A file in the spec repository, copied as is. For `link`, a file or directory |
| `content` | string or value | none | A string is written as is. Any other value is serialized to `format` |
| `target` | path | none | `link` only: a path outside the repository to point at |
| `mode` | string | `"managed"` | `"managed"`, `"link"`, `"seed"` or `"patch"` |
| `format` | string | from the extension | `"text"`, `"json"`, `"toml"`, `"yaml"`, `"ini"`, `"plist"` or `"lua"` |
| `owns` | dict | `{}` | `patch` only: dotted key paths and their values |
| `on_conflict` | string | `"ours"` | `patch` only: `"ours"`, `"theirs"` or `"fail"` when a key you own changed on disk |
| `on_drift` | string | `"overwrite"` | `managed` only: `"overwrite"` or `"fail"` when the file changed on disk |
| `perms` | string | the `src` file's mode, else the existing file's, else `"0644"` | Octal permission bits: `"0600"`, `"0440"`. No effect in `link` and `patch` mode |

Each mode takes its content one way:

| Mode | Takes |
| --- | --- |
| `managed`, `seed` | exactly one of `src` or `content` |
| `link` | exactly one of `src` or `target` |
| `patch` | `owns` only, on a JSON, TOML or YAML file |

The format comes from the path's extension (`.json`, `.toml`, `.yaml`, `.yml`, `.ini`,
`.plist`, `.lua`) unless `format` sets it. A dict or list `content` needs a format. `lua` writes
`return { ... }`.

### list_union, list_replace, list_keyed

```starlark
list_union(items)
list_replace(items)
list_keyed(field, items)
```

Strategies for a list under `file(owns = ...)`:

| Builtin | Result |
| --- | --- |
| `list_union(items)` | Adds the items that are missing, keeps what the app added, removes items you added earlier and no longer list |
| `list_replace(items)` | The list is exactly `items` |
| `list_keyed(field, items)` | Merges dicts by the value of `field`; every item needs it |

## secret

```starlark
secret(path, src, perms = "0400", owner = None, identities = [])
```

An age-encrypted file from the repository, decrypted at apply. See [Secrets](../guides/secrets.md).

| Argument | Type | Default | Meaning |
| --- | --- | --- | --- |
| `path` | string | required | Where the plaintext goes: absolute or `~/...` |
| `src` | path | required | The `.age` file in the repository |
| `perms` | string | `"0400"` | Octal permission bits |
| `owner` | string | none | `user` or `user:group` |
| `identities` | list of paths | the defaults | Private keys to decrypt with, tried in order |

Default identities: in the system scope `/etc/ssh/ssh_host_ed25519_key`,
`/etc/ssh/ssh_host_rsa_key`, then `~/.config/fspec/age.key`; in the user scope
`~/.ssh/id_ed25519`, `~/.ssh/id_rsa`, then `~/.config/fspec/age.key`.

## service

```starlark
service(name, exec = None, user = False, restart = "no", after = [], env = {},
        description = None, systemd = {}, launchd = {}, display_manager = False)
```

A service, rendered for the host's init system. See [Services and timers](../guides/services.md).

| Argument | Type | Default | Meaning |
| --- | --- | --- | --- |
| `name` | string | required | Letters, digits and `_ . - @` |
| `exec` | string | none | The command. Without it, the service is a unit the distro ships, which fspec only enables |
| `user` | bool | `False` | A user service instead of a system one |
| `restart` | string | `"no"` | `"no"`, `"on_failure"` or `"always"` |
| `after` | list of strings | `[]` | `"network"`, another service's name, or (systemd) a literal `*.target` |
| `env` | dict | `{}` | Environment variables |
| `description` | string | none | One line |
| `systemd` | dict | `{}` | systemd directives; `unit`, `service` and `install` are dicts merged into those sections |
| `launchd` | dict | `{}` | Raw plist keys merged over the ones fspec writes |
| `display_manager` | bool | `False` | systemd: this unit is the login screen, so the host boots to `graphical.target` |

A service without `exec` takes only `name`, `user` (systemd) and `display_manager`.

## timer

```starlark
timer(name, exec, schedule, user = False, env = {})
```

A scheduled job: a systemd timer, a cron entry or a launchd calendar interval.

| Argument | Type | Default | Meaning |
| --- | --- | --- | --- |
| `name` | string | required | No `.` on cron inits |
| `exec` | string | required | The command |
| `schedule` | string | required | Five-field cron, or `@hourly`, `@daily`, `@weekly`, `@monthly` |
| `user` | bool | `False` | Run as the user |
| `env` | dict | `{}` | Environment variables |

## user

```starlark
user(name, uid = None, shell = None, groups = [])
```

| Argument | Type | Default | Meaning |
| --- | --- | --- | --- |
| `name` | string | required | The account name |
| `uid` | int | none | Pin the UID so file ownership survives a reinstall |
| `shell` | string | none | A name (`"fish"`), resolved through `/etc/shells`, or a path |
| `groups` | list of strings | `[]` | Supplementary groups. Linux only. fspec adds missing groups and never removes one |

On Linux a missing account is created with `useradd`, and an existing one gets the declared UID and shell with `usermod`. On macOS the account must exist; fspec
sets its login shell and checks the UID.

## defaults

```starlark
defaults(domain, key, value, system = False)
```

A macOS `defaults` key. `value` is a bool, int, float, string, list or dict. `system = True`
writes the machine-wide domain in `/Library/Preferences` as root.

```starlark
defaults("com.apple.dock", "autohide", True)
defaults("NSGlobalDomain", "KeyRepeat", 2)
```

## setting

```starlark
setting(tool, key, value)
```

A macOS setting behind a command instead of a defaults domain.

| `tool` | Runs | `value` |
| --- | --- | --- |
| `"pmset"` | `pmset -a <key> <value>` | a number, a string without spaces, or a bool written as `1`/`0` |
| `"systemsetup"` | `systemsetup -set<key> on\|off` | a bool |
| `"firewall"` | `socketfilterfw --set<key> on\|off` | a bool |

```starlark
setting("pmset", "sleep", 0)
setting("firewall", "globalstate", True)
```

## builder

```starlark
builder(name, routes = [name])
```

A machine that builds `git(...)` packages for a host, reached over ssh. `routes` are ssh
destinations for the same machine, tried in order.

## sync, json_keys, keyring

```starlark
sync(name, peers, paths = [], include = [], exclude = [], keys = None, keyring = [],
     mode = "live", conflict = "newest", after = [], on_receive = None, unless_running = [])
json_keys(path, keys)
keyring(schema = None, attrs = {}, label = None)
```

App data shared between your hosts over ssh. See [Sync between hosts](../guides/sync.md).

| Argument | Type | Default | Meaning |
| --- | --- | --- | --- |
| `name` | string | required | Letters, digits, `-` and `_` |
| `peers` | list of host names | required | Every host that holds the item, this one included |
| `paths` | list of paths | `[]` | Files, or directories ending in `/`, under `~/` |
| `include`, `exclude` | list of globs | `[]` | Filter files inside a directory |
| `keys` | `json_keys(...)` | none | Sync only these top-level keys of a JSON file |
| `keyring` | list of `keyring(...)` | `[]` | Secret Service items |
| `mode` | string | `"live"` | `"live"`: every peer writes. `"seed"`: copied once to a host that has none |
| `conflict` | string | `"newest"` | `"newest"` or `"manual"` |
| `after` | list of item names | `[]` | Items that must sync first |
| `on_receive` | string | none | A shell command run after the item changed on this host; `{key}` is a key's new value |
| `unless_running` | list of process names | `[]` | Skip the item while one of these runs on either host |

An item needs at least one of `paths`, `keys` or `keyring`. `keyring` needs a `schema` or
`attrs`; an attribute value ending in `*` matches a prefix.

## Helpers

| Builtin | Returns |
| --- | --- |
| `glob(pattern)` | Sorted paths of the files under the spec directory that match, relative to it. No `..`, no absolute patterns |
| `origin()` | In a file loaded with `gh:`, `struct(url = "https://github.com/<owner>/<repo>", rev = "<locked commit>")`. `None` in your own files |

## dev.star

### dev

```starlark
dev(packages = [], libs = [], bins = [], env = {}, services = [], shell_hook = None,
    container = None, fallback_container = None, platforms = [...])
```

| Argument | Type | Default | Meaning |
| --- | --- | --- | --- |
| `packages` | list of `pkg(...)` or `pypi(...)` | `[]` | Tools on the project's `PATH` |
| `libs` | list of `lib(...)` | `[]` | System libraries, by pkg-config module |
| `bins` | list of `bin(...)` | `[]` | System executables |
| `env` | dict | `{}` | Environment variables |
| `services` | list of `service(...)` | `[]` | Processes or containers that run while a shell is open |
| `shell_hook` | string | none | sh code run after the environment is set; what it exports is kept |
| `container` | image | none | Run the shell in this image, such as `debian:13` |
| `fallback_container` | image | none | Run in this image only on machines that cannot provide a `lib` or `bin` |
| `platforms` | list | all four | The platforms the project supports |

`dev.star` has a global `platform` with `name`, `os`, `arch`, `is_linux` and `is_darwin`.

### lib, bin

```starlark
lib(name, apt = None, dnf = None, zypper = None, pacman = None, xbps = None, moss = None, brew = None)
bin(name, ...)
```

`lib("gtk4")` is the pkg-config module `gtk4`, or with a `.so` name a shared object.
`bin("cmake")` is an executable. `fspec update` asks each package manager which package
provides it. Name the package by hand where that lookup cannot: `lib("x", apt = "libx-dev")`.

### service (in dev.star)

```starlark
service(name, exec = None, version = None, image = None, ports = [], env = {})
```

Without `exec`, the image `<name>:<version>` (or `image`), pinned by digest, run with each port
published on `127.0.0.1`. With `exec`, a command from the project's environment.

## secrets.star

```starlark
kyan = "age1..."
laptop = "ssh-ed25519 AAAA..."

secrets = {
    "secrets/openai.age": [kyan, laptop],
    "secrets/wifi.age": [kyan],
}
```

Maps each `.age` file, relative to the spec directory, to the public keys it is encrypted to:
age recipients (`age1...`) or ssh public keys (`ssh-ed25519 ...`, `ssh-rsa ...`). `fspec secret
edit` and `fspec secret rekey` read it.
