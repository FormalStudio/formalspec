+++
title = "Sync between hosts"
description = "Move app data a spec cannot declare, such as signed-in accounts, keyring secrets and picture folders, between your own machines with fspec sync."
+++

`fspec sync` copies app data between your own machines over ssh. You use it for state that an
app writes and a spec cannot declare: a signed-in browser profile, a token in the keyring, a
wallpaper folder, one key of a JSON file the app owns. It runs peer to peer. There is no server,
no git remote and no relay.

Prerequisites:

- Every host that holds an item is a `host(...)` in the same spec and runs an fspec at least as
  new as the one you run.
- Each host can reach the others over ssh, the same way a [remote apply](remote-hosts.md) does.
- `secret-tool` on every host that syncs [keyring items](#sync-keyring-items). It is Linux only.

## When to use sync and when to use a file

| You want | Use |
| --- | --- |
| A config file whose content you write yourself | [`file(...)`](files.md) in the spec |
| A config file the app rewrites, where you own some keys | `file(..., mode = "patch")` |
| The same secret on every host, under your control | [`secret(...)`](secrets.md) |
| State the app creates and changes: logins, profiles, caches you want to keep, a picture folder | `sync(...)` |
| A secret the app stores in the keyring after you sign in | `sync(..., keyring = [...])` |

A file in the spec is the same on every host after `fspec apply`, and the repository holds its
content. A sync item is never in the repository. Its content lives only on your machines, and
`fspec sync` moves it between them when you run it.

## Declare an item on both hosts

An item names every host that holds it in `peers`, this one included. Each of those hosts lists
the same item, with the same arguments. The easiest way to guarantee that is one list in a
shared file that every host loads.

1. Create `sync.star` next to `spec.star`:

   ```starlark
   PAIR = ["laptop", "desktop"]

   def sync_items():
       return [
           sync("wallpapers", peers = PAIR, paths = ["~/Pictures/Wallpapers/"]),
           sync("notes", peers = PAIR, paths = ["~/Notes/"], exclude = ["*.tmp", ".trash"]),
       ]
   ```

2. Load it in `spec.star` and add the items to both hosts:

   ```starlark
   load("//sync.star", "sync_items")

   hosts = [
       host(name = "laptop", platform = "x86_64-linux", system = fedora(), sync = sync_items()),
       host(name = "desktop", platform = "x86_64-linux", system = arch(), sync = sync_items()),
   ]
   ```

   A `sync(...)` in `modules` joins the host's `sync` list, so a module can return items too.

3. Check what would move, from either host:

   ```sh
   fspec sync --dry-run
   ```

4. Sync:

   ```console
   $ fspec sync
   wallpapers: 41 files to desktop
   notes: 3 files to desktop, 2 files from desktop
   ```

`fspec sync` always acts as this machine, chosen by host name the same way `apply` chooses it.
With no arguments it syncs every item this host lists with every peer of each item. Name items
to sync only those:

```sh
fspec sync notes
```

`--peer desktop` limits the run to one peer when an item has more than two.

An item whose definition differs between two hosts fails, naming both:

```
fspec: notes with desktop: sync(notes) differs between laptop and desktop, or desktop does not list it
```

## Choose what an item carries

An item needs at least one of `paths`, `keys` or `keyring`. The full argument list is in
[builtins](../reference/builtins.md), under `sync`.

### Paths

`paths` are files, or directories when they end in `/`. Every path starts with `~/` and resolves
against each host's own home, so `/home/alex` on one host and `/Users/alex` on another hold the
same item.

```starlark
sync("steam-login", peers = PAIR, paths = [
    "~/.local/share/Steam/config/loginusers.vdf",
    "~/.local/share/Steam/config/config.vdf",
], mode = "seed", unless_running = ["steam"])
```

Only regular files sync. Symlinks, sockets and empty directories are skipped.

### Include and exclude

`include` and `exclude` are globs on the paths inside a directory item.

- A pattern without `/` matches any path component. `Cache` drops every directory or file named
  `Cache` at any depth, and `*.log` drops every log file.
- A pattern with `/` matches from the directory you declared. `Default/Code Cache` drops that one
  directory and leaves `Profile 1/Code Cache` alone.
- With `include`, only matching files sync. `exclude` then removes from what `include` kept.

```starlark
sync("discord", peers = PAIR, paths = ["~/.config/discord/"],
     exclude = ["Cache", "Code Cache", "GPUCache", "modules"],
     mode = "seed", unless_running = ["Discord"])
```

### JSON keys

`keys = json_keys(file, [keys])` syncs only those top-level keys of a JSON object. The rest of
the file stays each host's own. Use it for a state file where one setting should follow you and
the others are per machine.

```starlark
sync("active-wallpaper", peers = PAIR,
     keys = json_keys("~/.local/state/shell/state.json", ["wallpaper"]),
     after = ["wallpapers"],
     on_receive = "shellctl wallpaper set {wallpaper}")
```

Two arguments make this work:

- `after = ["wallpapers"]` syncs the folder first. While `wallpapers` did not sync in this run
  (its peer was unreachable, or it failed), this item waits and prints
  `active-wallpaper: waits for wallpapers, skipped`. Without it, the key could point at an image
  the host does not have yet. `after` applies only between items in the same run.
- `on_receive` is a shell command that runs on a host after the item changed there. `{key}` is
  that key's new value on the host, shell-quoted. Here it tells the running shell to load the
  new wallpaper. A placeholder that is not one of the item's keys is an error when the spec is
  evaluated.

The command runs with `sh -c` as you. If it fails, the item fails with `<item>: on_receive`.

## Sync keyring items

`keyring = [keyring(...)]` syncs items of the Secret Service, the desktop keyring that GNOME
Keyring and KWallet provide. `keyring(schema = ..., attrs = {...}, label = ...)` matches every
item with that schema and all of those attributes, and that label when you give one. An
attribute value that ends in `*` matches a prefix.

```starlark
sync("gh", peers = PAIR, keyring = [keyring(
    schema = "org.freedesktop.Secret.Generic",
    attrs = {"service": "gh:github.com", "username": "alex"},
)])
```

The Secret Service searches by attribute, so each `keyring(...)` needs a `schema` or at least
one attribute without `*`. To find the attributes an app uses, search for its items:

```sh
secret-tool search --all service gh:github.com
```

Secrets are read and stored with `secret-tool`. They travel only through the pipes of the ssh
session, never through a file on either host. The login keyring must be unlocked on both hosts;
a locked one fails the item with `the keyring on desktop is locked; log in there first`.

A keyring item is never deleted by a sync. A host that lacks one gets it.

### Chromium and Electron profiles need their key

Chromium, Chrome, Brave, Helium and Electron apps encrypt cookies and saved
logins with a key they keep in the keyring (`Chromium Safe Storage`). A profile synced without
that key opens signed out on the other host, since its cookies cannot be decrypted.

Sync the key as its own item, after the profile:

```starlark
def safe_storage(application):
    return keyring(schema = "chrome_libsecret_os_crypt_password_v2",
                   attrs = {"application": application})

def browser():
    return [
        sync("chromium", peers = PAIR, paths = ["~/.config/chromium/"],
             exclude = ["Cache", "Code Cache", "GPUCache"],
             mode = "seed", unless_running = ["chromium"]),
        sync("chromium-key", peers = PAIR, keyring = [safe_storage("chromium")],
             mode = "seed", after = ["chromium"]),
    ]
```

A separate item keeps the profile moving while the other host is logged out and its keyring is
locked. The key follows on the next run with both keyrings open.

Flathub apps keep their secret through the portal, one item per app id:
`keyring(schema = "org.freedesktop.portal.Secret", attrs = {"app_id": "com.spotify.Client"})`.

## Live and seed items

`mode` decides how two copies combine.

| Mode | Use it for | What happens |
| --- | --- | --- |
| `live` (default) | Data either host may change: folders, plain tokens that do not rotate, JSON keys | Each file, keyring item and key merges on its own against the last sync |
| `seed` | Single-writer data: LevelDB and SQLite profiles, OAuth refresh tokens that rotate | Copied whole to a host that has none of it, then each host keeps its own |

A browser profile is the typical `seed` item. Its databases break when files from two copies
mix, and a refresh token that rotates on one host is invalid on the other a minute later. After
the first copy, each host signs in on its own schedule and the copies drift apart, which is
what those apps expect.

When both hosts hold a seed item, `fspec sync` leaves it alone:

```
chromium: seeded on both hosts, each keeps its own; hand it over with `fspec sync chromium --from <host>`
```

### Hand an item over with --from

`--from` copies an item from one host over the other's copy, whole, without merging. Use it to
reseed a host, for example after you reinstalled it and signed in to nothing yet:

1. Close the app on both hosts.
2. On either host, run:

   ```sh
   fspec sync chromium chromium-key --from laptop
   ```

`--from` must name this host or the peer. On a `live` item it does the same: the named host's
copy replaces the other's, deletions included.

### Items an app holds open

`unless_running` lists process names, matched exactly against your own processes with
`pgrep`. While one runs on either host, the item is skipped:

```
discord: Discord is running on desktop, skipped
```

Set it on every item whose files an app keeps open, so a sync never copies a database while it
is being written.

## Conflicts

A `live` item compares each file, keyring item and JSON key against its base: what both hosts
held at the end of their last sync together.

| Changed on | Result |
| --- | --- |
| One host | Copied to the other, a deletion included |
| Both hosts, `conflict = "newest"` (default) | The later modification time wins; a change beats a deletion |
| Both hosts, `conflict = "manual"` | Nothing moves; each host keeps its own version |

With `manual`, `fspec sync` prints each conflicting path and exits non-zero:

```
notes: ~/Notes/todo.md changed on both hosts; left as it is on each
Error: 1 conflicting change(s) left as they are; set conflict = "newest" or hand one side over with --from
```

To resolve it, pick the host whose version you keep and hand the item over from there with
`--from`. The first sync of an item has no base, so a file both hosts hold with different
content counts as changed on both.

## Preview with --dry-run

`--dry-run` prints what would move and changes nothing on either host:

```console
$ fspec sync --dry-run
wallpapers: in sync with desktop
notes: 1 file, 1 deletion to desktop
gh: 1 keyring item from desktop
```

`to desktop` is what this host sends, `from desktop` is what it receives. Counts cover files,
deletions, keyring items and keys.

## Undo a sync with --rollback

Before a host replaces or deletes anything, it moves the old version aside. A replaced keyring
item stays in the keyring under `fspec-prev:` attributes, which no app looks up.

To put the previous version back on this host:

```console
$ fspec sync notes --rollback
notes: 4 restored
```

Rollback swaps the previous and current versions, so running it again swaps them back. It acts
on this host only and forgets the item's bases, so the next sync merges from scratch, as if it
were the first. Without item names it rolls back every item this host lists. `--rollback`
cannot be combined with `--from` or `--watch`.

Only the version before the last sync that changed something is kept.

## Run it continuously with --watch

`fspec sync --watch` syncs, waits, and syncs again until it is stopped. The interval is 60
seconds, or the number of seconds you give. A failing round is printed and the next one runs
anyway.

Run it as a user [service](services.md) on each host:

```starlark
service(
    "fspec-sync",
    exec = "/usr/local/bin/fspec --spec /home/alex/spec/spec.star sync --watch 300",
    user = True,
    restart = "always",
    after = ["network"],
)
```

Give the full path of `fspec` and of the spec, since a service has neither your `PATH` nor your
working directory. A seed item does nothing once both hosts hold it, so a running watcher is
safe for those too.

## What not to sync

Data bound to one device must not be synced. Each host links or registers itself instead:

- Signal and its linked-device clients: every host is its own linked device.
- Matrix clients with end-to-end encryption, Beeper included: device keys belong to one session
  on one device.
- 1Password and other password managers that register each device.
- ssh host keys, machine IDs and anything else that identifies the machine.

Copying one of these makes two machines claim the same identity. The service then logs one of
them out, or worse, the encryption keys fall out of step.

Caches are worth excluding too. They are large, change constantly and rebuild on their own.

## How it works

### Transport

To reach a peer, fspec tries the `routes` of this host's
[`builder`](../reference/builtins.md#builder) of the same name when there is one (a Tailscale name,
then a LAN address), else the peer's `ssh` destination, which defaults to its name. It probes each
route like a remote apply does: the peer must run the platform its host declares and an fspec at
least as new as this one. The distro is not checked on either end, since an item touches only the
home. A host declared as the distro it is moving to still syncs from the one it runs.

A peer that no route reaches is skipped with `<item>: desktop is unreachable, skipped`, and the
run still succeeds. On the far end, `fspec sync-agent` handles one request per ssh call. Files
travel as a tar inside ssh, which authenticates both ends with their host keys and encrypts the
stream. A copy keeps the sender's modification time.

### State

Each host keeps its sync state per item under `~/.local/state/fspec/sync/<item>/`:

| Path | Holds |
| --- | --- |
| `peers/<peer>.json` | The base shared with that peer: hashes, sizes and modification times. Never file contents or secrets; JSON key values are the exception |
| `prev/` | The files the last sync replaced or deleted, for `--rollback` |
| `incoming/` | Files on their way in, removed once placed |

A file whose size and modification time match the base is not hashed again, so a sync of a large
folder that did not change is fast.

## Troubleshooting

### A sync item fails on one peer

The error is printed per item and peer, and the run ends with a summary:

```
fspec: helium-key with desktop: the keyring on desktop is locked; log in there first
Error: sync failed for helium-key with desktop
```

Log in on that host, or unlock its keyring, and run the sync again.

### fspec is missing or older on the peer

```
fspec is not installed on desktop (not on PATH for a non-interactive ssh shell); install it there first, e.g. `cargo install --git https://github.com/FormalStudio/formalspec fspec`
```

```
fspec on desktop is older than this one (engine 2 < 3) and may not read this spec; install a current fspec there first
```

Install or update fspec on the peer. [Install fspec](../install.md#make-it-reachable-over-ssh)
covers the `PATH` of a non-interactive ssh shell.

### The item is not on this host

```
Error: host laptop has no sync item notes
```

The name you passed is not in this host's `sync` list. Add the item to the host, or check the
spelling.

```
Error: sync(notes) lists peers desktop, server without this host, laptop
```

An item reached this host through a module, but its `peers` does not name it. Add the host to
`peers`, or keep the item off this host.

### Items wait on each other

```
Error: sync items wait on each other in a cycle: a, b
```

Two items name each other in `after`. Remove one of the two entries.
