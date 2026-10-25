+++
title = "Users and macOS settings"
description = "Manage accounts, login shells and groups, and set macOS defaults, power, firewall, apps and fonts."
+++

This page covers the parts of a machine that are not packages or files: user accounts, and the
macOS settings that live in `defaults` domains or behind commands such as `pmset`. All of them are
declared in the spec, show drift in `plan`, and are put back by a failed apply.

## Declare a user

`user(...)` manages an account. List it in the host's `users`, or return it from a module:

```starlark
host(
    name = "desktop",
    platform = "x86_64-linux",
    system = fedora(),
    users = [user("alex", uid = 1000, shell = "fish", groups = ["wheel", "video", "docker"])],
    modules = [...],
)
```

| Argument | Default | Meaning |
| --- | --- | --- |
| `name` | required | The account name: letters, digits and `_ - .`, not starting with `-` |
| `uid` | none | Pin the UID, so file ownership survives a reinstall |
| `shell` | none | The login shell, by name or path |
| `groups` | `[]` | Supplementary groups. Linux only |

A shell name such as `"fish"` resolves to the entry in `/etc/shells` whose last component is
`fish`, and to `/usr/bin/fish` when none is listed. Give a full path for a shell elsewhere, such
as `"/opt/homebrew/bin/fish"`.

Removing a `user` stops managing it and leaves the account, its home and its groups in place.

### On Linux

- A missing account is created with `useradd -m`, with the `uid` and `shell` you set. That happens
  in the users phase, before files and secrets are written, so a [file](files.md) or a
  [secret](secrets.md) in the same apply can belong to the new account.
- For an account that exists, fspec changes its UID and shell with `usermod` after the packages are
  installed, because the shell usually comes from a package in the same apply.
- Groups are joined with `usermod -aG` at the very end of the apply, after every other phase.
  Packages create groups such as `docker` or `nordvpn` while they install, so one apply both
  installs the package and puts you in its group. fspec adds missing groups and never removes
  one.
- On a systemd host, a group that is still missing then gets one `systemd-sysusers` run, which
  creates groups that a package or a `file` declared in `/etc/sysusers.d`. A group that still does
  not exist is skipped with a warning and joined on a later apply:

  ```text
  fspec: user:alex: group nordvpn does not exist yet, joining it on a later apply
  ```

A new group membership applies to new logins. Log out and in again, or reboot.

`plan` reports `account alex is missing`, `uid is 1001`, `shell is /bin/bash` or
`not in group docker` as drift.

### On macOS

fspec never creates macOS accounts. The account must exist; fspec sets its login shell with
`chsh` and checks that its UID matches `uid`. It does not manage groups on macOS:

```starlark
users = [user("alex", shell = "/opt/homebrew/bin/fish")],
```

## Set defaults

`defaults(domain, key, value)` sets one key of a macOS preferences domain, like
`defaults write`:

```starlark
DEFAULTS = {
    "com.apple.dock": {"autohide": True, "tilesize": 48, "orientation": "bottom"},
    "com.apple.finder": {"ShowPathbar": True, "FXPreferredViewStyle": "clmv"},
    "NSGlobalDomain": {"KeyRepeat": 2, "InitialKeyRepeat": 15, "AppleShowAllExtensions": True},
}

def macos_defaults(host):
    return [defaults(d, k, v) for d, keys in DEFAULTS.items() for k, v in keys.items()]
```

```console
$ fspec plan
host laptop
  + defaults:com.apple.dock autohide  true
  + defaults:com.apple.dock orientation  "bottom"
  ...
```

`domain` is a bundle id or `NSGlobalDomain`. `value` is a bool, int, float, string, list or dict,
and keeps its type: fspec writes `-bool`, `-int`, `-float` or `-string`, and a list or dict with
every element typed, so `[1, 2]` stays a list of integers. `None` is refused; remove the line to
delete the key.

A user `defaults` key is in the user scope and written as you. `system = True` writes the
machine-wide domain in `/Library/Preferences` instead, as root, in the system scope:

```starlark
defaults("com.apple.SoftwareUpdate", "AutomaticallyInstallMacOSUpdates", False, system = True),
```

fspec journals the previous value, so a failed apply restores it. Removing a `defaults` line
deletes the key only while it still holds the value fspec wrote; a key you changed since stays,
with a message. `plan` shows `value is ...` or `key is missing` as drift.

fspec does not restart apps. The Dock, Finder and most apps read their preferences at launch, so
run `killall Dock` (or log out) to see a change.

## Change power, remote login and firewall settings

Some macOS settings have a command and no `defaults` domain. `setting(tool, key, value)` covers
three of those tools. All of them run as root, in the system scope.

| `tool` | Runs | `value` |
| --- | --- | --- |
| `"pmset"` | `pmset -a <key> <value>` | A number, a bool (written `1` or `0`), or a word |
| `"systemsetup"` | `systemsetup -set<key> on` or `off` | A bool |
| `"firewall"` | `socketfilterfw --set<key> on` or `off` | A bool |

```starlark
setting("pmset", "sleep", 0),
setting("pmset", "womp", 1),
setting("systemsetup", "restartfreeze", True),
setting("systemsetup", "remotelogin", True),
setting("firewall", "globalstate", True),
setting("firewall", "stealthmode", True),
```

`pmset` keeps a value per power source (battery, AC, UPS). fspec sets all of them with `-a` and
reports drift when any source differs. A failed apply restores each source's previous value.

Removing a `setting` stops managing it and leaves the value as it is. Turning a setting back has
no safe default: switching remote login off would cut an ssh session.

`systemsetup -setremotelogin` needs Full Disk Access for the terminal that runs fspec. Grant it in
System Settings, Privacy and Security, Full Disk Access.

## Install app bundles

A package that ships a `.app` bundle lists it in `apps`, and fspec copies the bundle into
`/Applications`, where Spotlight, Launchpad, the Dock and `open -a` find it:

```starlark
pkg("kopuz", source = github("Kopuz-org/kopuz", asset = "*.dmg"), apps = ["Kopuz.app"]),
```

On APFS the copy is a clone and takes no extra space. It is replaced when the package changes and
removed with it. [Packages](packages.md) covers `apps` and `.dmg` assets.

Apps that Homebrew packages as casks install with the `brew` source:

```starlark
pkg("ghostty", source = brew(cask = True)),
```

Homebrew refuses to run as root, so brew packages are in the user scope. See
[Packages](packages.md) for taps and formulae.

## Run background jobs

launchd agents and daemons are `service(...)` and `timer(...)`. A user service becomes a plist in
`~/Library/LaunchAgents`, a system service one in `/Library/LaunchDaemons`. Raw plist keys go in
`launchd = {...}`. See [Services and timers](services.md).

## Install fonts

A font is a file. Copy it into `/Library/Fonts` for every user, or `~/Library/Fonts` for you only:

```starlark
file("/Library/Fonts/Inter-Regular.otf", src = "fonts/Inter-Regular.otf"),
file("~/Library/Fonts/Inter-Bold.otf", src = "fonts/Inter-Bold.otf"),
```

Removing the line deletes the font, as long as it is still the file fspec wrote. On Linux, put
fonts from a package under `/usr/local/share/fonts` with `pkg(share = {...})`; see
[Packages](packages.md).

## Troubleshooting

| Error | Fix |
| --- | --- |
| ``user:alex does not exist; fspec never creates macOS accounts`` | Create the account in System Settings first |
| ``user:alex has uid 502 but the spec says 501; fspec does not change macOS accounts`` | Fix `uid` in the spec, or drop it |
| ``user:alex sets groups, but fspec only manages the login shell of macOS accounts`` | Remove `groups` on macOS hosts |
| ``user:alex has the invalid group `a,b` `` | One group per list item |
| ``defaults domain `/Library/Preferences/x` must be a bundle id or NSGlobalDomain`` | Use the bundle id, and `system = True` for `/Library/Preferences` |
| ``a defaults value cannot be null; remove the line to delete the key`` | `None` is not a value. Delete the line |
| ``setting:systemsetup remotelogin cannot be 1`` | `systemsetup` and `firewall` take `True` or `False` |
| ``setting:pmset sleep: could not read it from ...`` | `pmset -g custom` does not list that key on this Mac. Check the key name with `pmset -g custom` |
