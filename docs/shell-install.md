# Installing from the shell

How an app installed from FormalShell's launcher lands in a declarative spec, and how one is run
once without landing anywhere. This is fspec's half of the launcher's Install mode
(FormalShell `docs/superpowers/specs/2026-10-29-launcher.md` §8), which asks for four things:

1. a search call (id, name, source, version, icon, description, installed, per package);
2. an install and an uninstall call, each taking a package id and returning an operation id at
   once;
3. the operation stream ([activity.md](activity.md));
4. an owner query: which fspec package, if any, put a given desktop file there.

On top of those, `fspec run` runs a package once, the way `nix run` did.

Status: `fspec run` for store sources and Flathub apps and a first `fspec search` are built; everything else here
is design, for FormalOS plan task 4.1.

## The rule this keeps

The spec is the truth. An install from the shell is a commit to the spec, then an apply of that
commit; nothing is installed behind the spec's back, so the next `fspec apply` from any machine
keeps it, and removing the line removes the app. A run once is the opposite: the spec never
hears of it, and nothing it fetched outlives `fspec gc`.

## The apps module

Two files in the spec repo, created by `fspec app init`:

`apps.star`, the data fspec edits. Nothing else in the repo writes it, and fspec rewrites it
whole in this shape, so comments in it are not kept:

```python
# Written by `fspec app`.
APPS = {
    "e1504g": [
        {"pkg": "org.mozilla.firefox", "source": "flatpak", "added": "2026-10-29"},
        {"pkg": "btop", "source": "zypper", "added": "2026-10-29"},
    ],
}
```

`modules/apps.star`, plain Starlark written once, which the spec's author owns from then on:

```python
load("//apps.star", "APPS")

def apps(host):
    return [pkg(a["pkg"], source = _SOURCES[a["source"]](a["pkg"])) for a in APPS.get(host.name, [])]
```

(with `_SOURCES` mapping each source name to its builtin). Each host that installs from the
shell adds `apps` to its `modules` once. No builtin, field or lock key is new, so `ENGINE` does
not move.

Entries are per host: an app installed on e1504g lands on e1504g. `fspec app install --host h`
or `--hosts all` writes other hosts' lists. `fspec app install` refuses a spec whose host does
not get the new package once evaluated (`e1504g does not load modules/apps.star`), which is
checked by evaluating, not by reading `host.star`.

A package that the spec already declares elsewhere is refused as a duplicate by the plan, as two
declarations of one name always are; the search row reads it as installed.

## Where the commit is made

`fspec app` edits the checkout named by `$FSPEC_SPEC`, the variable plan and apply already
read; the shell's environment carries it. Without it every `fspec app` call fails with
`set FSPEC_SPEC to the spec checkout installs commit to`, and the launcher shows that as its one
dim row. A host applied only over ssh, as e1504g is from the macbook today, has no checkout: the
spec declares a clone for it in its home module, and `FSPEC_SPEC` points there.

An install is, under the host's apply queue lock (`~/.local/state/fspec/queue/<host>.lock`, so
two installs and an apply take turns, the second showing `queued`):

1. `git fetch`; the base is the upstream branch's tip.
2. Read `apps.star` and `fspec.lock` from the base commit, not the working tree, add the entry,
   and resolve only the new package into the lock (`fspec update <pkg>`'s code).
3. Write a commit from a temporary index (`git hash-object -w`, `git update-index --cacheinfo`
   under `GIT_INDEX_FILE`, `git write-tree`, `git commit-tree -p <base>`): subject
   `feat(apps): install firefox on e1504g`. No ref moves yet, and uncommitted edits in the
   checkout are never read, staged or swept in.
4. `fspec apply --rev <commit> --host <this host> --scope <the entry's scope>`.
5. On success, push the commit to the upstream branch. A rejected push means the branch moved:
   redo steps 2 and 3 on the new tip (the edit is data, so it is regenerated, never rebased) and
   push that. Offline, the commit is kept under `refs/fspec/apps/<host>` and pushed by the next
   `fspec app` call.
6. Fast-forward the local branch when it was the base, and write `apps.star` and `fspec.lock`
   into the working tree only where they still equal the base's copy; a file someone is editing
   is left alone and named.

On failure in step 4 the journal has rolled the machine back and no ref ever pointed at the
commit, so the spec and the machine both stay as they were.

An install applies the spec as of its commit, the same thing `fspec apply` would: changes pushed
from another machine but not yet applied here go in with it, and the stream's progress counts
them. The plan is printed in the result for the Activity row's details.

### Another machine must not undo it

Today `fspec apply --host e1504g` from the macbook converges e1504g to whatever the macbook's
checkout says, so a checkout that has not pulled would uninstall Firefox. With installs on the
host, a remote apply gets a guard: the host records the commit it last applied (pushed sha) in
its user state, `fspec facts --json` reports it, and the driving fspec refuses a spec commit that
does not contain it (`git merge-base --is-ancestor`): `e1504g was last applied from 1a2b3c4,
which this checkout does not have; pull first`. `--dirty` applies keep working, since the dirty
tree is on top of a commit that has it.

## Search

```
fspec search <query> [--json] [--limit 30] [--source flatpak|distro|store]
```

Answers from local catalogs only, so a keystroke never waits on the network. One JSON line per
package, best match first:

```json
{"id":"flatpak:org.mozilla.firefox","name":"Firefox","source":"flatpak","version":"144.0.2","icon":"/home/kyan/.cache/fspec/catalog/flathub/icons/org.mozilla.firefox.png","description":"Fast, private and safe web browser","installed":false,"run":false}
```

`id` is `<source>:<name>` and is what install, uninstall and run take. `icon` is a theme icon
name, an absolute path, or `null` (the launcher's `package` icon). `run` says whether
`fspec run` can start it.

The catalogs are the sources the apps module can write for this host, under
`~/.cache/fspec/catalog/`:

| Source | Catalog | Scope it installs in |
| --- | --- | --- |
| `flatpak` (Flathub) | Flathub's AppStream (`appstream.xml.gz` and its 128px icons) | user (`flatpak --user`) |
| `distro` (`zypper`, `pacman`, `dnf`, `apt`, `xbps`) | the distro's AppStream catalog (`/usr/share/swcatalog`) for apps, the package manager's own search for the rest | system |
| `store` (aqua, `github`) | the aqua registry's index | system |
| `brew` (macOS) | formulae.brew.sh's JSON | system |

Built so far (`fspec_sources::catalog`, which also writes the suggestions under a name that does
not resolve): `--json`, `--limit` and `--offline`, with `id`, `name`, `source`, `description`,
`run` and `spec` per line. Its catalogs are the aqua registry and Flathub's app ids (`/api/v2/appstream`,
ids only, so no name or description yet) kept a day under `~/.cache/fspec/catalog/`, Homebrew's
own API cache, the distro's package search, and crates.io and npm asked per query with a 2s
timeout, which `--offline` leaves out for the launcher. Not built yet: `version`, `icon`,
`installed`, `--source`, AppStream catalogs and the background refresh; a stale catalog is fetched
again in the foreground and the stale copy used when that fails.

A catalog older than a day is refreshed in the background by the search that finds it stale;
that search answers from the old one. `installed` is true when this host's state holds the
package, or the package manager has it (`rpm -q`, `flatpak list`), so a distro's base apps read
installed too. Desktop apps (an AppStream `desktop-application`) rank above command-line tools of
the same name.

## Install and uninstall

```
fspec app install <id> [--detach]
fspec app remove <id> [--detach]
```

Without `--detach` they run in the foreground like `apply`. With it, fspec forks the work into a
process of its own (a transient user unit, `systemd-run --user --unit fspec-app-<op id>`, where
systemd runs; else a `setsid` child), waits until that operation is listening, prints
`{"op":"<id>"}` and exits. That is the launcher's "returns an operation id at once": the shell
then follows `<id>.sock` on the stream, and a shell restart does not stop the install.

On the stream an install is kind `install` (`remove` for an uninstall), source
`fspec:app:<id>`, title `Installing Firefox`, with `keys` holding `pkg:<name>` throughout. Its
phases are `queued`, `resolving` (the lock entry), `committing` with item `spec` for the commit,
then the apply's own phases, then `pushing`. These kinds and phases are additions within
version 1 of the protocol.

`fspec app remove` takes the entry out of `apps.star` the same way. A package declared anywhere
but `apps.star` is refused (`firefox is declared in modules/linux/packages.star; remove it
there`), which is what the owner query's `removable` tells the launcher before it offers the
action. Removal trashes the app's data as any removal does; installing it again before
`fspec gc` restores it.

## Owner query

```
fspec owner <path> [--json]
```

```json
{"path":"/var/lib/flatpak/exports/share/applications/org.mozilla.firefox.desktop","id":"flatpak:org.mozilla.firefox","pkg":"org.mozilla.firefox","source":"flatpak","scope":"user","removable":true}
```

`id` and `pkg` are `null` when no fspec package owns the file; `removable` is true only for an
entry in this host's `apps.star`. fspec finds the package from where the file is:

- under a Flatpak `exports/share/applications`: the file's stem is the app id;
- a link under `/usr/local/share/applications`: its target is in a store path, which a state
  record names;
- anywhere else: the package manager's owner query (`rpm -qf`, `pacman -Qoq`, `dpkg -S`,
  `xbps-query -o`), matched against the state's distro packages;
- on macOS, a bundle in `/Applications` that a state record copied there.

## Privileges

- The edit, the commit, the push and the user scope run as the user. The spec is never written
  as root.
- A Flathub install is a user-scope change and `fspec app` applies only the scope its entry
  lives in, so it never asks for a password.
- A distro or store package needs the system scope. From a terminal that stays `sudo`, as today.
  With no terminal (stdin is not a tty, which is how the shell runs it), fspec hands the system
  scope to `pkexec` instead, with the same arguments `sudo_system` already passes, so the prompt
  is the shell's polkit agent's. The engine package ships the action:

  ```xml
  <action id="dev.formalsnake.fspec.system-scope">
    <description>Change system software</description>
    <defaults><allow_active>auth_admin_keep</allow_active></defaults>
    <annotate key="org.freedesktop.policykit.exec.path">/usr/local/bin/fspec</annotate>
  </action>
  ```

  `auth_admin_keep` is one prompt per five minutes, so an install and its undo do not prompt
  twice. pkexec drops the environment, so `SSH_AUTH_SOCK` does not reach the system scope and a
  `git(...)` package with no CI build cannot use builders from a shell install; it fails saying
  so, and installs from a terminal still can.
- A cancelled prompt fails the operation with `authorisation cancelled`, before anything
  changed.

## Rollback

- **Undo an install or removal**: `fspec app undo [<id>]` makes the inverse commit for the last
  `fspec app` change of that package on this host and applies it, through the same steps as an
  install. An undone removal brings the app back with its data from the trash. This is what the
  Activity row's `Undo` and the sidebar's rollback row run, since the spec stays the truth.
- **Roll the machine back**: `fspec rollback` repoints the previous generation as it always has.
  The spec still holds the app, so `plan` reports it as drift until the spec is reverted; the
  shell offers `fspec app undo` instead of `rollback` for anything `fspec app` did.
- **A failed install** needs neither: see step 4 above.

## Run once

```
fspec run <pkg> [--bin <name>] [--update] [--detach] [-- <args>]
```

Runs a package without adding it to the spec, the verb that replaces `nix run` and the
launcher's old `:nix` route.

- `<pkg>` is a store package as `dev.star` writes one: `ripgrep` (aqua), `github:sharkdp/bat`,
  `npm:cowsay`, `cargo:xh`, or a search id of those sources.
- fspec writes a one-package `dev.star` under `~/.cache/fspec/dev/once/<hash>/`, resolves its
  `fspec.lock` there the first time (`--update` resolves again), and activates it exactly as
  `fspec dev` would: the package unpacks into the user store (`~/.local/share/fspec/store`, or
  the system store's copy when it already has one), with its commands in a link farm. Nothing is
  written to the spec, the apps module or any state file, and no privilege is needed.
- The command run is `--bin`, else the package's only command, else the one named after the
  package; otherwise it fails listing them.
- In a terminal it runs in the foreground with the terminal, and fspec exits with its status;
  a command that cannot start fails the operation. `--detach` is how a GUI app is
  launched: the command starts in a session of its own with no terminal and fspec exits once it
  has started, so the shell starts `fspec run --detach <pkg>` the way it starts any app (its own
  scope, the activation token in the environment) and the app outlives fspec.
- On the stream it is kind `run`, source `fspec:run:<pkg>`, title `Running ripgrep`:
  `resolving` while it has no lock, `preparing` while it unpacks, and `succeeded` with summary
  `started` when the command starts. The operation ends at the start, not when the app exits.
- **How long it lives**: each run touches its link farm. `fspec gc --older-than <age>` drops run
  farms not used within the age, then the user-store packages no farm links into, the same rule
  `fspec dev` projects follow, and deletes the run's `dev.star` and lock with them. A package
  later installed for real is a separate system store path and is unaffected.

- **A Flathub app** (`flatpak:org.mozilla.firefox`) is installed into fspec's own flatpak
  installation, flatpak's per-user one moved by `FLATPAK_USER_DIR` to
  `~/.cache/fspec/dev/flatpak/installation` with Flathub added there, and `flatpak run` from it.
  The user's `flatpak list`, their exported desktop files and the spec never see it. Runtimes the
  system installation has are used from there; any other is downloaded beside the app. The
  download is the run's `preparing` phase, its progress in unit `percent` with the app's name as
  `item`, and a `downloading NN%` line in a terminal. Each run touches `used/<id>`; `fspec gc
  --older-than` uninstalls the apps not run within the age and the runtimes left unused, and the
  whole installation once no app remains.
- **A bare name aqua lacks** runs the one package of that exact name the catalogs offer
  (`fspec_sources::catalog::pick`): `firefox` prints `firefox: running flatpak:org.mozilla.firefox`
  and runs it. An app of the name (Flathub, Homebrew, the distro) outranks npm and crates.io,
  where short names are often squatted, and only a runnable one is picked; with none or two the
  run fails listing the candidates, runnable rows first. The choice is kept under
  `~/.cache/fspec/dev/names/` until `--update` or `fspec gc`.
- **Distro and Homebrew packages** cannot run without being installed: unprivileged zypper
  resolves no dependencies (`install --dry-run` wants root), and the packages assume their own
  paths under `/` (Firefox's launcher sets `MOZ_DIST_LIB=/usr/lib64/firefox`). `fspec run` refuses
  them naming the `pkg(...)` to add to the spec before `fspec apply`, and search rows from those
  sources carry `run: false`.
