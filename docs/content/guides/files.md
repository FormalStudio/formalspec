+++
title = "Files"
description = "Write, link, seed or merge config files, generate them from Starlark data, and handle drift."
+++

`file(...)` puts a file on the machine: a config copied from the spec repository, one generated
from Starlark data, a symlink, or a set of keys merged into a file an app also writes. You pick
how fspec treats the file with its `mode`. A path under `~/` belongs to the user scope and is
written as you; an absolute path belongs to the system scope and is written as root.

## Choose a mode

| Mode | On disk | Use it when |
| --- | --- | --- |
| `managed` (the default) | A real file. Its hash is kept in state | You own the whole file. Shell rc files, editor configs, unit files, anything in `/etc` |
| `link` | A symlink into the spec repository or to a path outside it | You want edits to land in the repository at once, or the file is a directory, or it lives on another drive |
| `seed` | Written once, if nothing is there | The app needs a starting file and owns it from then on |
| `patch` | The app's own file; fspec owns only the keys you list | The app rewrites the file itself and you only care about a few settings |
| `absent` | Nothing: a file or link at the path is removed | A file left by hand or by an installer must go, such as a temporary sudoers grant |

Copies are the default because they survive apps that rename, chmod or lock their config, and the
hash in state finds an edit as well as a symlink would.

## Write a file

A `managed` file takes its bytes from exactly one of two places:

- `src`: a file in the spec repository, relative to the directory that holds `spec.star`.
- `content`: a string written as is, or a Starlark value serialized to a format (next section).

```starlark
file("~/.gitconfig", src = "home/gitconfig"),
file("~/.config/git/ignore", content = ".DS_Store\n*.swp\n"),
file("/etc/sudoers.d/10-wheel", content = "%wheel ALL=(ALL) ALL\n", perms = "0440"),
```

`target` is for `link` mode only (see [Link a file or directory](#link-a-file-or-directory)).

Moving a `managed` file's `src` to another path in the repository with the same bytes is not a
change. The plan shows nothing and the next apply records the new path without writing the
file.

## Generate a config from Starlark data

When `content` is a dict or a list instead of a string, fspec serializes it. The format comes
from the path's extension, or from `format` when the path has none that fits.

| Extension | `format` | Notes |
| --- | --- | --- |
| `.json` | `"json"` | Pretty-printed with two spaces |
| `.toml` | `"toml"` | No `None` values: TOML has no null |
| `.yaml`, `.yml` | `"yaml"` | |
| `.ini` | `"ini"` | Top-level scalars first, then one `[section]` per nested dict. One level of sections, scalar values only |
| `.plist` | `"plist"` | XML plist. No `None` values |
| `.lua` | `"lua"` | Written as `return { ... }` |
| any other | `"text"` | Takes a string `content` only |

Keys keep the order you wrote them in, except that TOML puts plain values before tables.
Because the data is ordinary Starlark, you can share values between files and compute them per
host. This module writes one theme into five configs, and a plist on macOS:

```starlark
THEME = {"name": "nord", "font": "JetBrains Mono", "size": 12}

def theme_files(host):
    return [
        file("~/.config/app/theme.json", content = THEME),
        file("~/.config/app/theme.toml", content = {"theme": THEME, "plugins": ["git", "lsp"]}),
        file("~/.config/app/theme.yaml", content = THEME),
        file("~/.config/app/theme.ini", content = {"version": 2, "theme": THEME}),
        file("~/.config/app/theme.lua", content = THEME),
    ] + ([file("~/Library/Preferences/app.plist", content = THEME)] if host.platform.is_darwin else [])
```

The files fspec writes:

```json
{
  "name": "nord",
  "font": "JetBrains Mono",
  "size": 12
}
```

```toml
plugins = [
    "git",
    "lsp",
]

[theme]
name = "nord"
font = "JetBrains Mono"
size = 12
```

```yaml
name: nord
font: JetBrains Mono
size: 12
```

```ini
version = 2

[theme]
name = nord
font = JetBrains Mono
size = 12
```

```lua
return {
  name = "nord",
  font = "JetBrains Mono",
  size = 12,
}
```

For a format fspec does not serialize, build the string yourself with Starlark's `%`, `.format()`
and `"\n".join(...)`, or `json.encode(...)` for a JSON value inside another file.

## Set permissions

`perms` takes octal permission bits as a string: `"0600"`, `"0755"`, `"0440"`. Without it, a
`managed` or `seed` file gets the mode of its `src` file in the repository, else the mode the
file already has on disk, else `0644`. An executable script in the repository stays executable.

`perms` has no effect in `link` and `patch` mode. A patched file keeps the mode it has.

A file has no `owner` field. A user-scope file belongs to you; a system-scope file belongs to
root. [Secrets](secrets.md) take an `owner`.

## Drift

Drift is a difference between the file on disk and what the last apply wrote. `fspec plan` shows
it with `!`:

```console
$ fspec plan
host laptop
  ! file:~/.config/app/theme.json drifted: content changed
  ! file:~/.gitconfig drifted: source changed
  0 to add, 2 to change, 0 to remove
```

| Note | Meaning |
| --- | --- |
| `content changed` | Someone edited the file on disk |
| `source changed` | The `src` in the repository no longer matches what was written |
| `file is missing` | The file was deleted |
| `replaced by a symlink` | A symlink now sits where fspec wrote a file |
| `link is missing`, `link points elsewhere` | A `link` was deleted or repointed |
| `symlink replaced by a regular file` | An app saved over a `link` |
| `links <path>, the spec links <path>` | A `link` was applied from another checkout of the spec, such as a temporary clone |
| `` `ui.theme` changed ``, `` `ui.theme` was removed `` | An owned key of a `patch` file no longer holds what fspec wrote |

By default the next apply writes the file again. To keep the edit, take it into the spec first
(see [Keep an edit with fspec adopt](#keep-an-edit-with-fspec-adopt)).

For a file you never want overwritten by surprise, set `on_drift = "fail"`:

```starlark
file("~/.config/app/theme.json", content = THEME, on_drift = "fail"),
```

Apply then stops and rolls back instead of writing over the edit. `on_drift` applies to
`managed` files only.

## Keep an edit with fspec adopt

`fspec adopt <path>` copies the file on disk back into the repository and updates state, so the
file stops reading as drifted. Pass the path as the spec writes it or as a path on disk.

```console
$ fspec adopt ~/.gitconfig
copied ~/.gitconfig into /home/alex/spec/home/gitconfig
```

Then review and commit the change in the repository.

What `adopt` does depends on the mode:

| Mode | Result |
| --- | --- |
| `managed` with `src` | Copies the file into `src` |
| `managed` with `content` | Prints a diff (spec on the left, disk on the right) and exits with an error. Edit the Starlark to match |
| `link` with `src` | When an app replaced the link with a regular file, copies that file into `src` |
| `link` with `target` | Nothing to do |
| `patch` | Lists the owned keys that changed on disk. Change `owns` to match |
| `seed` | Refused: fspec never manages a seed after the first write |
| `absent` | Refused: there is no content to adopt |

## Link a file or directory

A `link` takes exactly one of `src` or `target`.

With `src`, the link points into the spec repository's working copy, stow style. `src` may be a
file or a directory. Edits through the link land in the repository at once:

```starlark
file("~/.config/nvim", mode = "link", src = "home/nvim"),
```

The link holds the absolute path of the working copy. If you move the repository, `plan` shows
`link points elsewhere` and the next apply relinks.

With `target`, the link points outside the repository, at an absolute or `~/` path. The target may
be a directory, and it may not exist yet, like a folder on a drive that is not mounted. The link
dangles until it is:

```starlark
file("~/.gradle", mode = "link", target = "/Volumes/External/gradle"),
file("~/.local/share/fonts", mode = "link", target = "~/Sync/fonts"),
```

A real directory already at the path, such as one an app made before the spec declared the link,
goes to fspec's trash before the link is made, and a rollback puts it back.

Some apps save a config by writing a new file and renaming it over the old one, which replaces
the symlink with a regular file. The next apply handles it:

- A link to a file in the repository: fspec copies the new file's content into the repository,
  then puts the link back. Your change is kept, in git.
- A link to a `target`: fspec puts the link back and copies nothing. It owns only the link.

## Seed a file once

A `seed` is written when nothing exists at the path, and never touched again. Use it for a file
an app needs to start with and rewrites on its own:

```starlark
file("~/.config/app/state.json", mode = "seed", content = {"first_run": False}),
```

A seed never shows drift, and changing its `src` or `content` changes nothing on disk. Removing the
line deletes the file only while it still holds what fspec wrote.

## Remove a file that must not exist

`absent` takes only the path. Apply removes a file or symlink there (a directory is an error, so a
typo cannot delete a tree), and removes it again whenever it reappears:

```starlark
# Passwordless sudo for the first apply, until the spec's sudo PAM config is in place.
file("/etc/sudoers.d/99-first-apply", mode = "absent"),
```

Removing the line leaves the path alone. Inside a package's `files`, the removal waits for that
package: if it is deferred, so is the file.

## Patch keys in a file the app owns

`patch` mode edits JSON, TOML and YAML files. You list the keys you own in `owns`; every other key
stays as the app wrote it. A key is a dotted path into nested tables:

```starlark
file("~/.config/agent/settings.json", mode = "patch", owns = {
    "model": "opus",
    "ui.theme": "dark",
    "permissions.allow": list_union(["Bash(rg:*)", "Bash(fd:*)"]),
    "hooks.PostToolUse": list_keyed("matcher", [
        {"matcher": "Edit", "command": "fmt"},
    ]),
}),
```

`"ui.theme": "dark"` sets `theme` inside the `ui` table, creating the table when it is missing.
A dot always separates levels, so a key whose own name contains a dot cannot be owned. When the
file does not exist, apply creates it with only your keys.

A patch takes `owns` only, no `src` or `content`. The format comes from the extension or
`format`, and must be `json`, `toml` or `yaml`.

### How the three-way merge works

State keeps the value fspec last wrote for each owned key, the same idea as `kubectl apply`'s
last-applied annotation. On every apply, fspec compares three values per key: what the spec wants,
what is on disk, and what it wrote last.

| On disk | Meaning | Result |
| --- | --- | --- |
| Equals what fspec wrote last | Untouched | Write the new desired value |
| Differs | You or the app changed it | Decided by `on_conflict`, below |
| Missing | Deleted | Treated as a change |
| Present, and fspec never wrote it | The app set it first | A conflict, except for `list_union` and `list_keyed`, which join the existing list |

`on_conflict` picks what a conflict does:

| `on_conflict` | Result |
| --- | --- |
| `"ours"` (the default) | Your value wins. `plan` shows the key as drifted first |
| `"theirs"` | The app's value stays and fspec stops managing the key until you change its desired value in the spec |
| `"fail"` | Apply stops before anything changes |

Keys you never declared are never read or written. Dropping a key from `owns` deletes it from the
file only while it still holds the value fspec wrote last.

Rewriting a patched file serializes it again, so comments in it are lost.

### List strategies

A list under `owns` needs a strategy that says how it combines with what the app keeps there:

| Builtin | Result |
| --- | --- |
| `list_union(items)` | Adds the items that are missing and keeps items the app added. An item you listed before and no longer list is removed |
| `list_replace(items)` | The list is exactly `items` |
| `list_keyed(field, items)` | A list of dicts, matched by the value of `field`. Your item replaces the app's item with the same key; other items stay. Every item needs `field` |

A plain list value with no strategy behaves like `list_replace`.

## Watch for drift as it happens

`fspec watch` reports drift on every file the last apply wrote, in both scopes, as it happens. It
runs until you stop it with Ctrl-C:

```console
$ fspec watch
drift file:~/.config/app/theme.json: content changed
drift file:~/.gitconfig: content changed
in sync file:~/.gitconfig
```

It watches `managed`, `link` and `patch` files and secrets. For a patched file it stays quiet
about writes to keys you do not own, which makes it a quick way to find out which keys an app
rewrites. It reports nothing for files already in sync when it starts.

## Files that belong to a package

A file declared on a package goes with the package:

```starlark
pkg("limine", source = zypper(), files = [
    file("/boot/efi/limine.conf", src = "boot/limine.conf"),
]),
```

When you remove the `pkg` line, apply removes the package and deletes its files, as long as they
still match what fspec wrote. Which module returned a file never ties it to a package; only
`files = [...]` does. [Packages](packages.md) covers the rest of a package's removal.

## Removal

Deleting a `file` line deletes the file on the next apply, but only while it still holds what
fspec wrote (for a link: while it is still fspec's symlink). A file you or an app changed is kept,
with a warning:

```text
fspec: keeping /home/alex/.gitconfig: it changed since fspec wrote it
```

Removing a `patch` line takes out the keys that still hold fspec's values and leaves the file.

## Protect paths

`host(protect = [...])` lists paths fspec must never write under. A file, secret or app data path
under one fails the plan:

```starlark
host(
    name = "server",
    platform = "x86_64-linux",
    system = debian(),
    protect = ["/home", "/srv/data"],
    modules = [...],
)
```

The home of the user running fspec stays usable even when it sits under a protected path, so
`protect = ["/home"]` keeps fspec out of other users' homes and still writes yours.

## How it works

- `plan` evaluates the spec and compares each file with state and with the disk. It never writes.
- Apply renders every file into a staging directory first, with mode `0600`, so a format error or
  a `fail` conflict stops it before anything live changes.
- Each file is then written to a temporary file next to the target and renamed over it, so an app
  never reads a half-written config. The previous content is journaled; a failed apply puts it
  back.
- A dangling symlink among a file's parent directories, such as one left by home-manager into a
  `/nix/store` that no longer exists, is replaced by a real directory.
- Writing a file under `/etc/systemd/system`, `/etc/systemd/user` or `~/.config/systemd/user` runs
  `systemctl daemon-reload` (with `--user` in the user scope), so systemd sees the unit.
- State keeps the sha256 of each managed file, the link target of each link, and the last-applied
  value of each owned key. [Paths](../reference/paths.md) lists where the state files live.

## Troubleshooting

| Error | Fix |
| --- | --- |
| ``set exactly one of src and content`` | A `managed` or `seed` file needs one of the two. Remove the other |
| ``cannot infer a format from the path; set format`` | `content` is a dict or list and the extension is not in the format table. Add `format` |
| ``patch mode edits json, toml and yaml files; set format or use such an extension`` | Patch mode reads only those three formats |
| ``src /home/alex/spec/home/gitconfig does not exist`` | `src` is relative to the directory that holds `spec.star` |
| ``target rel/path must be absolute or start with ~/`` | A link `target` cannot be relative |
| ``perms `rw` is not an octal mode like "0644"`` | Write the mode as an octal string |
| ``toml has no null``, ``plist has no null`` | A `None` in `content` cannot be written to that format. Leave the key out |
| ``owned key `hooks.PostToolUse`: list_keyed item {"command":"fmt"} has no `matcher` field`` | Every `list_keyed` item needs the key field |
| ``/home/bob/x is under protected path /home`` | The path is under an entry in `protect` |
| ``... was changed outside fspec; run `fspec adopt ...` to keep the change, or set on_drift = "overwrite"`` | The file has `on_drift = "fail"` and was edited. Adopt the edit, or remove `on_drift` to let apply overwrite it. For a file generated from `content`, `adopt` prints the diff to copy into the spec |
| ``... changed since fspec last wrote `model` and on_conflict is "fail"`` | A patched key changed on disk. Run `fspec adopt <path>` to see which keys, then change `owns` or `on_conflict` |
