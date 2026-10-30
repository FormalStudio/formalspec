# Installing from the shell

How an app installed from FormalShell's launcher lands in a declarative spec, and how one is run
once without landing anywhere. This is fspec's half of the launcher's Install mode
(FormalShell `docs/superpowers/specs/2026-10-29-launcher.md` §8), which asks for four things:

1. a search call (id, name, source, version, icon, description, installed, per package);
2. an install and an uninstall call, each taking a package id and returning an operation id at
   once (`fspec app install --detach`, `fspec app uninstall --detach`);
3. the operation stream ([activity.md](activity.md));
4. an owner query: which fspec package, if any, put a given desktop file there (`fspec app owner`).

On top of those, `fspec run` runs a package once, the way `nix run` did.

Status: built.

## The rule this keeps

The spec is the truth. An install from the shell is a commit to the spec, then an apply of that
commit; nothing is installed behind the spec's back, so the next `fspec apply` from any machine
keeps it, and removing the line removes the app. A run once is the opposite: the spec never
hears of it, and nothing it fetched outlives `fspec gc`.

## The apps module

Two files beside the spec entry point, created by `fspec app init`:

`apps.star`, the data fspec edits. Nothing else in the repo writes it, and fspec rewrites it
whole, so comments in it are not kept. It is one assignment of a JSON-shaped literal, which is
valid Starlark and which fspec reads back without evaluating:

```python
# Written by `fspec app install` and `uninstall`, whole, so edits here are not kept. modules/apps.star turns it into packages.
APPS = {
    "e1504g": [
        {
            "name": "org.mozilla.firefox",
            "source": "flatpak",
            "ref": "org.mozilla.firefox",
            "added": "2026-10-30"
        }
    ]
}
```

`name` is the `pkg(...)` name, `source` the builtin (`flatpak`, `zypper`, `pacman`, `apt`, `dnf`,
`aqua`, `github`, `npm`, `cargo`, and on macOS `brew` and `cask`), `ref` what that builtin takes.
A host with no entries has no key.

`modules/apps.star`, plain Starlark written once, which the spec's author owns from then on: it
loads `APPS` and defines `apps(host)`, returning `pkg(a["name"], source = <builtin>(a["ref"]))` for
each of the host's entries. Each host that installs from the shell adds it once:

```python
load("//modules/apps.star", "apps")
host(..., modules = [..., apps])
```

No builtin, field or lock key is new, so `ENGINE` does not move.

Entries are per host: an app installed on e1504g lands on e1504g. `--host h` writes another
host's list and applies that host over ssh. An install whose package does not reach the host
once the edited spec is evaluated is refused before anything is committed (`host e1504g does not
load modules/apps.star; add apps to its modules ...`).

A package the host already declares is refused, matched by name and by what it installs (a
`pkg("parsec", source = flatpak("com.parsecgaming.parsec"))` blocks
`flatpak:com.parsecgaming.parsec`): `host g815 already declares parsec (...); it is installed by
the spec`. The search row reads it as installed by `spec`.

## Where the commit is made

`fspec app` edits the repo holding the spec it is given (`--spec`, a `spec.star` in the current
directory, then `$FSPEC_SPEC`, as plan and apply pick it). The shell's environment carries
`FSPEC_SPEC`. The checkout's branch needs an upstream (`origin/main`), which installs are pushed
to. A host applied only over ssh has no checkout of its own: the spec declares a clone for it and
points `FSPEC_SPEC` there: Kyan's spec clones FormalSnake/dotfiles to `~/.config/formalspec` with a
user oneshot and sets `FSPEC_SPEC` in `environment.d`.

Before each change fspec brings that checkout to the upstream tip it fetched (`git merge
--ff-only`), so the machine's copy is never older than what it applies. A checkout whose branch
has commits the upstream lacks is refused (`main in ~/.config/formalspec has commits origin/main
does not; push them ...`), as is one whose fast-forward an uncommitted edit blocks. A checkout on
another branch is left alone.

An install runs under the host's apply queue lock (`~/.local/state/fspec/queue/<host>.lock`), so
two installs and an apply take turns, the second showing `queued`:

1. `git fetch` of the upstream branch; the base is its tip (the last fetched tip when offline),
   or this host's unpushed commit under `refs/fspec/apps/<host>` when that is built on the tip.
2. `apps.star` read from the base commit, not the working tree; the tree of the base unpacked
   into a temp directory, `apps.star` rewritten there, and the new package alone resolved into
   that tree's `fspec.lock` (`fspec update <name>`'s code).
3. A commit written from a throwaway index (`git read-tree <base>`, `hash-object -w`,
   `update-index --cacheinfo`, `write-tree`, `commit-tree -p <base>`) under the user's own git
   identity: subject `feat(apps): install org.mozilla.firefox on e1504g`. No ref moves, and
   uncommitted edits in the checkout are never read, staged or swept in.
4. That commit applied for the host and the package's scope, as `fspec apply --rev <commit>
   --scope <scope>` would.
5. A push of the commit to the upstream branch. A rejected push means the branch moved: steps 1
   to 3 run again on the new tip (the edit is data, so it is regenerated, never rebased; the
   machine is not applied twice) and that commit is pushed, up to four tries. Any other push
   failure keeps the commit under `refs/fspec/apps/<host>`, says so, and the next `fspec app` call
   builds on it and pushes it. When the upstream branch has moved on without it by then, that
   call refuses, naming the `git merge` that puts it on the tip (a merge, since the host records
   that commit), rather than build on the
   tip and lose the unpushed change.
6. The checked-out branch fast-forwarded to the commit when it is the upstream branch and has
   nothing of its own; git refuses where an uncommitted edit would be overwritten, and fspec then
   says it left the branch as it was.

When step 4 fails the journal has rolled the machine back and no ref ever pointed at the
commit, so the spec and the machine both stay as they were.

An install applies the spec as of its commit, which is what `fspec apply` would do: changes pushed
from another machine but not yet applied here go in with it.

### Another machine must not undo it

`fspec apply --host e1504g` from the macbook converges e1504g to whatever the macbook's checkout
says, so a checkout that has not pulled would uninstall Firefox. After each push, `fspec app`
writes the commit to `~/.local/state/fspec/apps.json` on the host it changed (over ssh, as
`fspec app mark <commit>`, for `--host`), `fspec facts
--json` reports it as `apps`, and an apply refuses a spec that does not contain it (`git
merge-base --is-ancestor`), from another machine and from this one alike: `e1504g was last
changed by fspec app at 1a2b3c4d5e6f, which this checkout does not have; pull first`. A
`--dirty` apply is checked against its `HEAD`. A host whose fspec predates the field reports
none and is not checked.

## Search

```
fspec search <query> [--json] [--limit 30] [--offline]
```

One JSON line per package, best match first:

```json
{"id":"flatpak:org.mozilla.firefox","name":"Firefox","source":"flatpak","version":"159.0","icon":"/home/kyan/.cache/fspec/dev/flatpak/installation/appstream/flathub/x86_64/active/icons/128x128/org.mozilla.firefox.png","description":"Fast, Private & Safe Web Browser","installed":false,"owner":null,"installable":true,"run":true,"spec":"pkg(\"org.mozilla.firefox\", source = flatpak())"}
```

| Field | Meaning |
| --- | --- |
| `id` | `<source>:<ref>`, what `fspec app install`, `uninstall` and `fspec run` take |
| `name` | The catalog's display name where it has one (Flathub's AppStream), else the package name |
| `source` | `flatpak`, `zypper`, `pacman`, `apt`, `aqua`, `npm`, `cargo`, `brew` or `cask` |
| `version` | The catalog's newest version, or `null` where the catalog does not say (the distro's search, aqua) |
| `icon` | An absolute path to the catalog's 128px icon, or `null` (the launcher's `package` icon) |
| `description` | One line, or `null` |
| `installed` | Whether this machine has it, by any of the three owners below |
| `owner` | `spec` (a package in this machine's state, installed by an apply), `flatpak` (in `flatpak list`, outside the spec), `distro` (the package manager has it, outside the spec), or `null` |
| `installable` | Whether `fspec app install` can write this source for this machine: Flathub, the distro's own manager, aqua, github, npm and cargo on Linux; Homebrew, casks, aqua, github, npm and cargo on macOS |
| `run` | Whether `fspec run` starts it without installing |
| `spec` | The `pkg(...)` that declares it by hand |

`--offline` (the launcher's) leaves out crates.io and npm, the two catalogs asked over the network
per query, so a keystroke never waits on the network. The aqua registry and Flathub's id list are
fspec's own copies under `~/.cache/fspec/catalog/`, fetched again when a day old. Flathub's
AppStream catalog is flatpak's own, fetched in a process of its own by a search that finds none or
one a day old (`flatpak --user update --appstream flathub` against fspec's flatpak installation,
`~/.cache/fspec/dev/flatpak/installation`, so the user's installation is never touched); that
search answers from what it has, and the ones after it get names, summaries, versions and icons.
The newest of that copy, the user's and the system installation's is read, through an index under
`~/.cache/fspec/catalog/flathub-apps.json` read again only when the catalog changes.

`installed` costs one `flatpak list` and one `rpm -q` (`pacman -Qq`, `dpkg-query -W`) per search
for the rows shown, so it is right for distro base apps too.

## Info

```
fspec info <id> [--json]
```

One search hit's details for the launcher's Install pane, read where the catalog keeps them; a
field the catalog does not carry is `null`, never guessed:

```json
{"id":"flatpak:org.mozilla.firefox","source":"flatpak","name":"Firefox","version":"159.0","summary":"Fast, Private & Safe Web Browser","description":"When it comes to your life online ...","license":"MPL-2.0","developer":"Mozilla","download":"129.5 MB","installed":"345.2 MB","screenshot":"https://dl.flathub.org/repo/screenshots/org.mozilla.firefox-stable/752x423/...png","icon":"/home/kyan/.cache/fspec/dev/flatpak/installation/appstream/flathub/x86_64/active/icons/128x128/org.mozilla.firefox.png"}
```

A Flathub app's fields come off its own AppStream component in the same catalog search reads
(the description's paragraphs and list items as lines, the first screenshot's thumbnail closest
under 752px wide), and its two sizes off fspec's flatpak installation's cached summary
(`flatpak remote-ls --cached`), so nothing is fetched. A zypper package's come off `zypper
--no-refresh info`: name, version, summary, description and installed size; zypper has no
licence, developer, download size or screenshot to give. Any other source answers its id and
source. Reading the catalog whole costs about 1.5s, so the launcher asks once per row it shows.

## Install and uninstall

```
fspec app install <id> [--host <h>] [--detach]
fspec app uninstall <id> [--host <h>] [--detach]
```

Without `--detach` they run in the foreground like `apply` and print the commit (`feat(apps):
install org.mozilla.firefox on g815 1a2b3c4d5e6f`). With it, fspec starts the same command in a
session of its own (`setsid`, stdio closed, the operation id passed down), waits until that
operation is listening on the stream, prints `{"op":"<id>"}` and exits. That is the launcher's
"returns an operation id at once": the shell follows `<id>.sock`, and a shell restart does not
stop the install. The child stays in the caller's cgroup and so in its login session, which is
where polkit looks for the agent that answers a prompt. A command that fails before it publishes
fails the parent with its exit status.

On the stream an install is kind `install` (`uninstall` for an uninstall), source
`fspec:install:<id>`, title `Installing Firefox` (Flathub's name where its catalog is here, else
the package name), host the host it changes. Its phases are `resolving`, `committing` with item
`spec`, `evaluating`, the apply's own phases (`planning`, `preparing`, `committing`, per scope),
then `pushing`; the result is the apply's change count. These are additions within version 1 of
the protocol ([activity.md](activity.md)).

`fspec app uninstall` takes the entry out of `apps.star` the same way and applies the scope the
package was in. A package declared anywhere but `apps.star` is refused (`parsec (flatpak:...) is
declared in host g815's spec outside apps.star; take it out there`), which is what the owner
query's `removable` tells the launcher before it offers the action. Removal trashes the app's
data as any removal does; installing it again before `fspec gc` restores it. The lock entry stays
until the next `fspec update`.

`fspec app list [--json]` prints the host's entries (`id`, `name`, `added`), what the sidebar's
updates and rollback rows (Plan 5.3) read.

## Owner query

```
fspec app owner <path or desktop id> [--json]
```

```json
{"path":"/var/lib/flatpak/exports/share/applications/org.mozilla.firefox.desktop","id":"flatpak:org.mozilla.firefox","pkg":"org.mozilla.firefox","source":"flatpak","owner":"spec","removable":true}
```

A target with no `/` is a desktop file id (`org.mozilla.firefox`, with or without `.desktop`),
looked up in `$XDG_DATA_HOME`, the user's and the system's Flatpak exports and `$XDG_DATA_DIRS`.
The package behind the file is found from where it is:

- inside a store package once its links are followed (`/usr/local/share/applications` links into
  `/opt/fspec/store/<dir>`, the user store's into `~/.local/share/fspec/store/<dir>`): the package
  whose state record keeps that `store/<dir>`, always `spec`;
- under a Flatpak `exports/share/applications`: the file's stem is the app id;
- anywhere else: the package manager's owner query (`rpm -qf`, `pacman -Qoq`, `dpkg -S`).

`owner` is `spec` when a package in this machine's state installs that, else `flatpak` or
`distro`; with no package behind the file every field but `path` is `null`. `removable` is true
only for a `spec` package in this host's `apps.star`, and then `id` is that entry's.

## Privileges

- The edit, the commit, the push and the user scope run as the user. The spec is never written
  as root.
- Every source the module writes on Linux installs in the system scope (`flatpak()` installs
  `--system`), so an install changes it, as an apply does.
- The system scope runs as root through `sudo` when `sudo -n true` succeeds (no password asked)
  or stdin is a terminal (asked there, as today). With neither, which is how the shell runs it,
  fspec hands it to `pkexec` with the same arguments, so the prompt is the session's polkit
  agent's and fspec never holds a password. pkexec drops the environment, so `SSH_AUTH_SOCK`
  does not reach the system scope and a `git(...)` package with no CI build cannot use builders
  from a shell install; installs from a terminal still can.
- A refusal fails the operation before anything in the system scope changed: pkexec's 126 reads
  `system scope apply: authorisation cancelled; nothing in the system scope changed`, its 127
  `not authorised to change system software (no polkit agent answered, or the password was
  refused)`. The user scope and the commit are not touched.
- pkexec matches a polkit action's `exec.path` after resolving links, and
  `/opt/fspec/current/bin/fspec` resolves into the store, so a spec that wants a named prompt
  declares a real file for pkexec to run, `/usr/libexec/fspec/fspec` (mode 0755, `exec
  /opt/fspec/current/bin/fspec "$@"`). fspec runs that file when it exists and
  `/opt/fspec/current` is the running binary. The action beside it, at
  `/usr/share/polkit-1/actions/dev.formalsnake.fspec.policy`, names the prompt and keeps the
  authorisation five minutes (`auth_admin_keep`) so an install and its undo prompt once:

  ```xml
  <action id="dev.formalsnake.fspec.system-scope">
    <description>Change system software</description>
    <defaults><allow_active>auth_admin_keep</allow_active></defaults>
    <annotate key="org.freedesktop.policykit.exec.path">/usr/libexec/fspec/fspec</annotate>
  </action>
  ```

  Without it pkexec asks under its generic `org.freedesktop.policykit.exec` action, every time;
  an fspec run from anywhere else (a build, `~/.cargo/bin`) is asked for under that action too.

## Rollback

- **Undo an install or removal**: `fspec app undo [<id>] [--host <h>] [--detach]` finds the
  newest `feat(apps): install|uninstall <name> on <host>` commit touching `apps.star` on the
  upstream branch (of `<id>`'s package when given), reads the entry as that commit listed it, and
  runs the inverse (`fspec app uninstall` or `install` of that id) through the same steps. A
  reinstall before `fspec gc` brings the app's data back from the trash, as any reinstall does.
- **Roll the machine back**: `fspec rollback` repoints the previous generation as it always has.
  The spec still holds the app, so `plan` reports it as drift until the spec is reverted; the
  shell offers the inverse `fspec app` call instead of `rollback` for anything `fspec app` did.
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
  them naming the `fspec app install` that installs them, and search rows from those sources
  carry `run: false`.
