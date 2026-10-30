# The operation stream

`fspec apply`, `fspec update`, `fspec rollback`, `fspec run` and `fspec app` say what they are doing on a local socket while
they run, so a desktop shell can show them as live activity instead of a terminal. This is the
protocol, version 1. The write side is `crates/fspec-core/src/activity.rs`; `fspec activity`
prints it.

## Where

Each running operation listens on its own socket:

```
$XDG_RUNTIME_DIR/fspec/activity/<id>.sock
```

Without `XDG_RUNTIME_DIR` (macOS), the directory is `<temp dir>/fspec-<uid>/activity`. Both
directories are mode 0700; fspec refuses to publish into one that is not owned by the user or
that others can write. A reader may create the directory itself (same mode) so it has something
to watch before the first operation runs.

A reader:

1. Lists `*.sock` in the directory on start and connects to each. A refused connection is an
   operation that died without cleaning up; skip it (the next fspec run deletes it).
2. Watches the directory (inotify `IN_CREATE`, or any equivalent) and connects to each new
   socket. An operation can be shorter than any polling interval.
3. Reads lines until the operation closes the connection.

The socket goes away when the operation ends. One process is one operation: `apply --all` is
one operation across its hosts, and the system scope of an apply, which runs under
`sudo fspec`, relays its progress to its parent's socket rather than listening itself.

An apply of this machine driven from another machine (`fspec apply --host g815` on the macbook)
runs `fspec apply --local` on this machine over ssh, and that process publishes here: the ssh
session has the same `XDG_RUNTIME_DIR` as the desktop session.

## Lines

Every line is one JSON object ending in `\n`:

```json
{"v":1,"type":"snapshot","op":{...}}
{"v":1,"type":"update","op":{...}}
```

- The first line on every connection is a `snapshot`: where the operation is now. A shell that
  restarts mid-apply reconnects and reads it.
- Every `update` after it carries the whole operation again, never a delta. A reader replaces
  what it holds and can drop any line without going wrong. Updates are coalesced to at most one
  per 50 ms; a burst of steps reaches the reader as its latest state.
- The last line has `state` `succeeded` or `failed`, and the operation then closes the
  connection.
- A connection that ends without a final state means the operation died: show it as failed.
- A reader that does not keep up is cut off rather than waited on. Its last line can be cut
  short; discard a final line with no `\n`, reconnect if the socket still exists, and read the
  snapshot.

Readers never write. Lines with a `v` the reader does not know are skipped.

## The operation

```json
{
  "id": "1a1886361f4-71100",
  "kind": "apply",
  "source": "fspec:apply:g815",
  "title": "Applying g815",
  "host": "g815",
  "pid": 71100,
  "started": 1793289576948,
  "updated": 1793289577014,
  "state": "running",
  "phase": "committing",
  "scope": "system",
  "progress": {"done": 3, "total": 12, "unit": "resources"},
  "item": "zypper: firefox and git",
  "keys": ["pkg:firefox", "pkg:git"],
  "result": null,
  "error": null
}
```

| Field | Meaning |
| --- | --- |
| `id` | Opaque and unique, stable for the operation's life; the socket's file name |
| `kind` | `apply`, `update`, `rollback`, `run`, `install` or `uninstall` |
| `source` | Opaque. Operations with equal sources are one row: applies of one host, updates of one spec. `fspec:<kind>:<host>`, `fspec:update:<spec dir>`, `fspec:apply:all`, `fspec:run:<package>`, `fspec:install:<search id>`, `fspec:uninstall:<search id>` |
| `title` | A sentence for people: `Applying g815`, `Updating fspec.lock`, `Rolling back e1504g`, `Running ripgrep`, `Installing Firefox`, `Uninstalling Firefox` |
| `host` | The host being changed (an install's included), `null` for an update, a run and `apply --all` |
| `pid` | The fspec process |
| `started`, `updated` | Milliseconds since the epoch: when it started and when the step last changed |
| `state` | `running`, `succeeded` or `failed` |
| `phase` | What it is doing, below |
| `scope` | `system` or `user` while an apply or rollback is in one, else `null` |
| `progress` | `{done, total, unit}`, or `null` while the phase cannot say how far it is |
| `item` | What the step works on now, for people: a backend and its batch (`zypper: firefox, git and 3 more`), a package name, `g815 over ssh` |
| `keys` | The resource keys of the current step (`pkg:firefox`, `file:~/.config/x`), at most 32 |
| `result` | On success: `{changes, summary}`, the number of changes made and a sentence (`12 changes`, `no changes`, `3 lock entries changed`; a run's is `started`, with 0) |
| `error` | On failure: the error as fspec prints it |

`phase`, in the order an operation passes through them:

| Phase | Kind | Meaning |
| --- | --- | --- |
| `evaluating` | all | Reading the spec |
| `queued` | apply | Waiting behind another apply of the same host |
| `remote` | apply | Shipping the spec to a remote host and applying there; its progress is on that host's stream |
| `planning` | apply, rollback | Diffing the spec against the state, per scope |
| `preparing` | apply, rollback | Fetching, verifying and staging; nothing live has changed yet |
| `committing` | apply, rollback | Changing the machine, in phase order under the journal |
| `resolving` | update, run | Asking upstream for each package's newest version (a run only before its first start, or with `--update`) |
| `writing` | update | Writing `fspec.lock` |
| `resolving` | install, uninstall | Reading `apps.star` at the upstream tip and resolving the new package into `fspec.lock`; then `committing` (item `spec`) while the commit is written, then an apply's own phases |
| `pushing` | install, uninstall | Pushing the commit it applied to the spec's upstream branch |
| `preparing` | run | Unpacking into the user store and linking its commands, or downloading a Flathub app into fspec's flatpak installation; the run succeeds when the command starts, not when it exits |

`progress.unit` is `resources` (apply and rollback: one per resource the plan changes, counted in
each of `preparing` and `committing`), `packages` (update and run: one per package declaration) or
`percent` (a run of a Flathub app while flatpak downloads it, out of 100).
Progress starts again at 0 in each phase and each scope. `done` counts finished work, so `item`
is the work in progress past `done`.

## Versioning

Within version 1, fields, phases, kinds and units may be added. A reader ignores fields it does
not know, shows an unknown phase by its name, treats an unknown unit as a plain count and an
unknown kind like `apply`. Anything else (a field removed or changing meaning) is version 2,
sent with `"v":2`.

## What a shell does with it

FormalShell's Activity service (FormalOS plan, decisions 5, 6 and 12) maps it onto the Linux
live-activity standards it already reads:

- Progress is `done / total`, the same 0..1 that `com.canonical.Unity.LauncherEntry` sends and
  `round(100 * done / total)` for a notification's `value` hint. `progress: null` is an
  indeterminate ring.
- `source` is the grouping key: one row and one live toast per source, updated in place, with a
  count when more than one operation shares it (an apply queued behind a running one).
- `failed` is `urgent`. `title` with `result.summary` or `error` is the finished row, kept for a
  few seconds before it folds into the notification centre.
- `keys` lets an install started from the launcher find its own package (`pkg:firefox`) in the
  apply that installs it.

## Limits

- An operation started as root outside a user's apply (`sudo fspec apply`, a root timer)
  publishes in root's directory, which the user's shell does not read. Applies run as the user
  hand only the system scope to sudo, and that relays.
- Progress is per backend batch, not per byte: a package manager installing twenty packages in
  one transaction is one step from 0 to 20.
