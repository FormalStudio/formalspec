+++
title = "Services and timers"
description = "Declare long-running services and scheduled jobs once, and run them on systemd, launchd, runit, OpenRC, dinit or s6."
+++

`service(...)` describes a program that runs in the background: what to run, whether to restart
it, and what it needs first. fspec renders that description for the host's init system, whether
that is systemd, launchd on macOS, or runit, OpenRC, dinit or s6. `timer(...)` runs a command on a
schedule. You write each once and the same line works on every host, as long as it only uses
fields that host's init supports.

The host's init comes from its `system`: systemd for an `image(...)` host and most distros,
runit for `void()`, launchd on macOS, and whatever you pass as `init = ...` to a distro builtin.
See [Hosts and modules](../concepts/hosts-and-modules.md).

## Run a service

```starlark
service("syncthing", exec = "syncthing serve --no-browser",
        user = True, restart = "always", after = ["network"]),
```

On apply, fspec writes the unit, enables it so it starts at boot (or login), and starts it now.
The plan shows the command:

```console
$ fspec plan
host laptop
  + service:syncthing  syncthing serve --no-browser
  1 to add, 0 to change, 0 to remove
```

A changed service is restarted. A removed one is stopped, disabled, and its files deleted; on systemd its failed state is cleared too, so `systemctl --failed` does not keep listing a unit that no longer exists.

| Argument | Default | Meaning |
| --- | --- | --- |
| `name` | required | Letters, digits and `_ . - @`. Not starting with `.` or `-` |
| `exec` | none | The command line. Without it, see [Enable a unit the distro ships](#enable-a-unit-the-distro-ships) |
| `user` | `False` | A user service, run as you, instead of a system service run as root |
| `restart` | `"no"` | `"no"`, `"on_failure"` or `"always"` |
| `after` | `[]` | What must start first: `"network"`, another fspec service's name, or a systemd `*.target` |
| `env` | `{}` | Environment variables for the command |
| `description` | none | One line. systemd uses the name when it is unset; runit, s6 and launchd have no place for it |
| `systemd` | `{}` | Extra systemd directives (see [Escape hatches](#escape-hatches)) |
| `launchd` | `{}` | Extra launchd plist keys |

### System or user

A system service is in the system scope: fspec writes it as root and the init runs it as root.
`user = True` puts it in the user scope: it runs as you, under your login session, and its files
live in your home. Use a user service for anything that belongs to you, such as a sync client or
an agent that needs your files.

On systemd a user service is a user unit, started by your user manager at login. On macOS it is a
LaunchAgent in your GUI session. OpenRC and s6 have no user services.

### exec

`exec` is one line. A bare command name such as `syncthing` is looked up on `PATH` through
`/usr/bin/env`, since init systems do not search `PATH` themselves. Use a full path when the
command lives somewhere the init's `PATH` does not reach, such as `/opt/fspec/current/bin/x`.

Shell syntax (pipes, redirects, `$VAR`, globs, `~`) works: where the init wants an argument list,
fspec runs such a command through `/bin/sh -c`.

### restart

| `restart` | systemd | launchd | runit | OpenRC | dinit | s6 |
| --- | --- | --- | --- | --- | --- | --- |
| `"no"` | no `Restart=` | `RunAtLoad` only | a `finish` script runs `sv down .` | no supervisor, `command_background` | `restart = false` | `finish` exits 125 |
| `"on_failure"` | `Restart=on-failure` | `KeepAlive` with `SuccessfulExit = false` | `finish` stops it on exit status 0 | `supervise-daemon`, which restarts on any exit | `restart = on-failure` | `finish` exits 125 on status 0 |
| `"always"` | `Restart=always` | `KeepAlive = true` | the runit default | `supervise-daemon` | `restart = true` | the s6 default |

### after

| `after` entry | systemd | runit, s6 | OpenRC | dinit | launchd |
| --- | --- | --- | --- | --- | --- |
| `"network"` | `After=` and `Wants=network-online.target` (`After=` only for a user unit) | ignored | `need net` | `depends-on = network` for system services | ignored |
| another service's name | `After=fspec-<name>.service` | runit: `run` exits until `sv check` passes; s6: `dependencies.d/fspec-<name>` | `after fspec-<name>` | `depends-on = fspec-<name>` | ignored |
| `"graphical.target"` or another `*.target` | written as is | fails `plan` | fails `plan` | fails `plan` | ignored |

launchd has no ordering between jobs, so it ignores `after` and `description`.

### env

```starlark
service("web", exec = "/opt/web/bin/server --port 8080",
        restart = "on_failure", env = {"RUST_LOG": "info"}),
```

systemd gets `Environment=` lines, launchd `EnvironmentVariables`, runit, OpenRC and s6 `export`
lines in their scripts, and dinit a leading `/usr/bin/env RUST_LOG=info`. Names must be valid shell
variable names and values one line.

## Escape hatches

Features only one init has go under that init's own argument. On any other init, `plan` fails
and names the field instead of dropping it, so a spec never silently loses a setting.

### systemd = {...}

Each key is a `[Service]` directive. The keys `unit`, `service` and `install` take a dict of
directives for that section. A directive you set replaces the one fspec generated with the same
name, and a list repeats the directive. Booleans become `yes` and `no`.

```starlark
service(
    "web",
    exec = "/opt/web/bin/server --port 8080",
    restart = "on_failure",
    env = {"RUST_LOG": "info"},
    systemd = {
        "MemoryMax": "512M",
        "unit": {"StartLimitBurst": 3},
    },
),
```

fspec writes `/etc/systemd/system/fspec-web.service`:

```ini
# Managed by fspec. Edits are overwritten on the next apply.

[Unit]
Description=web
StartLimitBurst=3

[Service]
ExecStart=/opt/web/bin/server --port 8080
Restart=on-failure
Environment="RUST_LOG=info"
MemoryMax=512M

[Install]
WantedBy=multi-user.target
```

A oneshot that runs at boot and counts as active afterwards:

```starlark
systemd = {"Type": "oneshot", "RemainAfterExit": True,
           "unit": {"Before": "systemd-user-sessions.service"}},
```

A user service tied to the graphical session:

```starlark
service("bar", exec = "waybar", user = True, restart = "always", after = ["graphical-session.target"],
        systemd = {"install": {"WantedBy": "graphical-session.target"}}),
```

A unit with `RefuseManualStart` or `RefuseManualStop` under `unit` is enabled and disabled but
never started, stopped or restarted by apply. It starts with whatever wants it.

### launchd = {...}

Raw plist keys merged over the ones fspec writes. Booleans, numbers, strings, lists and dicts map to
their plist types.

```starlark
service(
    "sync-notes",
    exec = "/opt/fspec/current/bin/notes-sync",
    user = True,
    launchd = {
        "StartInterval": 300,
        "StandardOutPath": "/tmp/notes-sync.log",
        "ProcessType": "Background",
    },
),
```

`RunAtLoad = False, KeepAlive = False` gives a job that is loaded but idle until you run
`launchctl kickstart`. `WatchPaths`, `ThrottleInterval` and `LimitLoadToSessionType` go in the
same way.

A login item that opens an app once at login is a user service with the default
`restart = "no"`:

```starlark
service("login-notes", exec = "/usr/bin/open -g -j -a /Applications/Notes.app", user = True),
```

## Enable a unit the distro ships

Without `exec`, a service names a unit that a package already installed, and fspec only enables
it at boot:

```starlark
service("sshd"),
service("gdm", display_manager = True),
```

```console
$ fspec plan
host desktop
  + service:gdm  enable the distro unit
  + service:sshd  enable the distro unit
  2 to add, 0 to change, 0 to remove
```

fspec does not start a service, so a new display manager or sshd takes effect at the next boot,
and the same apply works inside the root `fspec switch --distro` builds. On systemd, a unit that
only activates something (`.swap`, `.mount`, `.automount`, `.timer`, `.path`, `.socket`) is
started too, so a declared swapfile or disk is up as soon as the apply finishes:

```starlark
service("swap-swapfile.swap"),
service("mnt-games.mount"),
```

If it fails to start (the disk is missing), the apply defers it and the next apply tries again.
Removing one disables it but leaves it active until the next boot, since unmounting a busy mount
would fail the apply.

| Init | Enable | Disable on removal |
| --- | --- | --- |
| systemd | `systemctl enable --force <name>`, which takes over an alias such as `display-manager.service` | `systemctl disable <name>` |
| runit | links `/etc/sv/<name>` into `/etc/runit/runsvdir/default` | removes the link |
| OpenRC | `rc-update add <name> default` | `rc-update del <name> default` |
| dinit | `dinitctl enable <name>` | `dinitctl disable <name>` |
| s6, launchd | not supported | |

Such a service takes no `restart`, `after`, `env`, `description` or `systemd`. On systemd it may
set `user = True` to enable a user unit, and the name may carry a suffix such as
`service("nordvpnd.socket")`. Drift is the unit no longer being enabled. Removing it disables the
unit and leaves the distro's files alone. An apply that fails after the forced enable rolls it
back: fspec disables the unit and puts back the alias links it took over (the `Alias=` names in
the unit file's `[Install]` section), so `display-manager.service` points at the old display
manager again.

`display_manager = True` (systemd only) marks the login screen. While it is declared, the host
boots to `graphical.target`. Some distros default to `multi-user.target`, which never starts a
display manager. Plan reports another default as drift, and removing the service puts back the
default it replaced.

## Services that belong to a package

A service declared on a package goes with it:

```starlark
pkg(
    "nordvpn",
    source = zypper(repo = "nordvpn"),
    services = [service("nordvpnd.socket"), service("nordvpnd")],
),
pkg("syncthing", services = [
    service("syncthing", exec = "syncthing serve --no-browser", user = True, restart = "always"),
]),
```

Removing the `pkg` line stops and removes its services and timers first, then uninstalls the
package. `pkg(timers = [...])` works the same way. See [Packages](packages.md).

## Restart after a package update

On systemd, apply restarts a running service whose program moved to a new store directory. When a
`pkg` update moves `/opt/fspec/current/bin/web` to a new version, the running process still
executes the old one. fspec compares the unit's `ExecStart` with the process's executable, and if
they resolve to different packages in the store, `plan` reports the service and `apply` restarts
it:

```console
$ fspec plan
host server
  ! service:web drifted: fspec-web.service runs 1a2b3c4d-web-1.4.0, the store now has 5e6f7a8b-web-1.5.0
  0 to add, 1 to change, 0 to remove
$ fspec apply
...
service:web: restarted, fspec-web.service runs 1a2b3c4d-web-1.4.0, the store now has 5e6f7a8b-web-1.5.0
```

This covers fspec's own services and distro units, system and user, including a user service that
runs a package from the system scope. A unit that refuses manual starts or stops is left for its
next start. The other inits do not restart a service for this.

## Run a command on a schedule

```starlark
timer("backup", exec = "restic backup ~/Documents", schedule = "30 2 * * *", user = True),
timer("gc", exec = "/usr/local/bin/fspec gc --older-than 14d", schedule = "@weekly"),
```

| Argument | Default | Meaning |
| --- | --- | --- |
| `name` | required | As for a service. No `.` on hosts whose timers are cron entries |
| `exec` | required | The command |
| `schedule` | required | Five cron fields (`minute hour day-of-month month day-of-week`), or `@hourly`, `@daily`, `@weekly`, `@monthly` |
| `user` | `False` | Run as you |
| `env` | `{}` | Environment variables, on every init |

Fields take numbers, `*`, ranges (`1-5`), lists (`1,15`), steps (`*/10`, `0-30/5`), and month and
day names (`jan`, `mon`). A schedule that restricts both day-of-month and day-of-week fails
`plan`: cron runs the job when either matches, systemd only when both do, so fspec refuses to
guess.

| Init | A timer becomes |
| --- | --- |
| systemd | `fspec-<name>.timer` with `OnCalendar=` and `Persistent=true`, plus a oneshot `fspec-<name>.service`. A run missed while the machine was off happens at the next boot |
| launchd | A `fspec.<name>` plist with one `StartCalendarInterval` entry per combination of listed values, at most 64 |
| runit, OpenRC, dinit, s6 | A cron entry, schedule as written |

A cron system timer is `/etc/cron.d/fspec-<name>`, run as root. A user timer is a block in your
crontab between `# BEGIN fspec:<name>` and `# END fspec:<name>`; fspec leaves the rest of the
crontab alone. fspec expects a cron that reads `/etc/cron.d`, such as cronie or vixie-cron.
dcron, busybox crond and snooze are not supported.

## Where each init's files go

| Init | System service | User service | Timer | Not supported |
| --- | --- | --- | --- | --- |
| systemd | `/etc/systemd/system/fspec-<name>.service` | `~/.config/systemd/user/fspec-<name>.service` | `fspec-<name>.timer` next to its `.service` | `launchd = {...}` |
| launchd | `/Library/LaunchDaemons/fspec.<name>.plist` | `~/Library/LaunchAgents/fspec.<name>.plist` | a `fspec.<name>` plist | services without `exec`, `systemd = {...}`. `after` and `description` are ignored |
| runit | `/etc/sv/fspec-<name>/run`, linked into `/etc/runit/runsvdir/default` | `~/.local/service/fspec-<name>/run` | cron | `*.target` in `after`, `systemd`, `launchd` |
| OpenRC | `/etc/init.d/fspec-<name>` | none | cron | `user = True`, `*.target`, `systemd`, `launchd` |
| dinit | `/etc/dinit.d/fspec-<name>` | `~/.config/dinit.d/fspec-<name>` | cron | `*.target`, `systemd`, `launchd` |
| s6 | `/etc/s6/sv/fspec-<name>/`, compiled into `/etc/s6/db/` | none | cron | `user = True`, services without `exec`, `*.target`, `systemd`, `launchd` |
| cron (timers on the four above) | `/etc/cron.d/fspec-<name>` | a block in the user's crontab | | `.` in the timer name |

systemd units, OpenRC scripts, dinit files and cron files start with
`# Managed by fspec. Edits are overwritten on the next apply.` An edit to any file fspec rendered
shows as drift (`... was edited`), and the next apply writes the file again.

### Init details

- **systemd.** Apply runs `systemctl daemon-reload`, then `enable` and `start` for a new unit or
  `restart` for a changed one (`--user` for a user unit). A user unit starts with your user
  manager, so it runs while you are logged in. A unit that fails to start does not undo the apply:
  fspec warns with the `journalctl -u` command to read, keeps the rest, and starts the unit
  again on the next apply.
- **launchd.** System jobs load into the `system` domain, user jobs into `gui/<uid>`, with
  `launchctl bootstrap`. A change runs `launchctl bootout`, then `bootstrap` again.
- **runit.** `runsvdir` picks up a new link within five seconds, so an add runs no command. A
  change runs `sv restart`, a removal `sv down`. User services are scanned by a system service
  `fspec-runsvdir-<user>`, created by the first user service, so that apply needs root. It stays
  when the last user service goes.
- **OpenRC.** Apply runs `rc-update add fspec-<name> default` and `rc-service fspec-<name> start`.
- **dinit.** Apply runs `dinitctl enable` and `start`; a change runs `reload`, then `restart`.
  `enable` needs the `boot` service to have a `waits-for.d` directory.
- **s6.** Every fspec service is also in the bundle `/etc/s6/sv/fspec`, which the host's boot
  bundle must include. Apply compiles `/etc/s6/sv` with `s6-rc-compile` into
  `/etc/s6/db/fspec-g<generation>-a`, switches with `s6-rc-update`, and starts the bundle with
  `s6-rc -u change fspec`. Old databases stay in `/etc/s6/db`.

A unit file you write yourself with [`file(...)`](files.md) under `/etc/systemd/system` or
`~/.config/systemd/user` also gets a `daemon-reload`, but fspec does not enable it. Pair it with
`service("<unit name>")` to enable it.

## Troubleshooting

| Error | Fix |
| --- | --- |
| ``service:web sets `systemd` (MemoryMax), which only systemd supports, and this host uses runit`` | Move the field into a module that only systemd hosts get, for example `if host.init == "systemd"` |
| ``service:web sets user = True, but OpenRC has no user services`` | Make it a system service on that host. s6 gives the same error |
| ``service:web orders itself after `graphical.target`, a systemd target, and this host uses runit`` | Drop the target on non-systemd hosts |
| ``service:gdm has no exec, and enabling a unit the system ships is not supported on launchd`` | Distro units exist on Linux only. s6 gives the same error |
| ``service:gdm has no exec, so it enables the distro's own unit, which takes no restart`` | A service without `exec` only takes `name`, `user` and `display_manager` |
| ``service:gdm has no exec, but /etc/sv/gdm does not exist; install the package that ships it`` | runit: add the package that ships the service directory |
| ``timer name `backup.daily` holds a `.`, which cron ignores in /etc/cron.d file names`` | Rename the timer |
| ``schedule `0 0 1 * 1` restricts both day-of-month and day-of-week; cron runs on either, and no init here can express that`` | Use two timers, or restrict one field |
| ``schedule `@yearly` is not supported, use @hourly, @daily, @weekly, @monthly or five cron fields`` | Write `0 0 1 1 *` |
| ``schedule expands to 70 launchd calendar entries, more than the 64 fspec writes`` | launchd: simplify the schedule, or use `launchd = {"StartInterval": ...}` |
| ``service name `my service` may only hold letters, digits and `_ . - @`, and must not start with `.` or `-` `` | Rename the service |
