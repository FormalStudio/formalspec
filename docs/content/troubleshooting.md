+++
title = "Troubleshooting"
description = "Errors fspec prints, what causes each one and how to fix it, organised by symptom."
+++

This page lists the errors you are most likely to hit, by symptom. Each section quotes the
message as fspec prints it, then gives the cause and the fix. Paths and names in the messages
are examples: yours show your own host, package and home.

fspec prints an error as `Error:` followed by the message. When the error happened inside a
step, the step comes first and the cause follows under `Caused by:`:

```
Error: preparing store

Caused by:
    pkg ripgrep is not in fspec.lock, run fspec update
```

Read the last line first. It is the actual cause.

## fspec update stops at the GitHub rate limit

```
Error: 1 problem(s):
resolving fd (github:sharkdp/fd): GET https://api.github.com/repos/sharkdp/fd/releases/latest: GitHub API rate limit reached (resets in 42 min; set GITHUB_TOKEN or run `gh auth login` for a higher limit)
```

**Cause.** `fspec update` asks the GitHub API for the newest release of every `github(...)` and
`aqua(...)` package. Without a token, GitHub allows 60 requests an hour.

**Fix.** Give fspec a token. It sends `GITHUB_TOKEN` when set, else what `gh auth token` prints:

```sh
gh auth login
fspec update
```

Packages that failed keep their previous entry in the lock, and every other package is still
updated, so you can also wait for the reset and run `fspec update` again.

## Apply tells you to run fspec update

```
Error: preparing store

Caused by:
    pkg ripgrep is not in fspec.lock, run fspec update
```

```
Error: preparing store

Caused by:
    pkg ripgrep has no artifact for aarch64-darwin in fspec.lock, run fspec update
```

```
pkg:discord has no locked flatpak commit; run `fspec update`
```

```
fspec.lock has pkg:fd from `aqua:fd` but not from `github:sharkdp/fd`; run `fspec update`
```

**Cause.** `plan` and `apply` read only the lock, and the lock does not have what the spec now
asks for. You added a package, changed its source, or added a host on a platform the lock has no
artifacts for. `fspec update` resolves each package for the platforms of the hosts that declare
it, so a lock written before a new `aarch64-darwin` host existed has no artifacts for it.

**Fix.** Run a full update and commit the lock with the spec:

```sh
fspec update
git add spec.star fspec.lock
git commit -m "lock ripgrep"
```

`fspec update <pkg>` updates only that package and leaves the others as they are, so after adding
a host run it without arguments.

## A package shows "patches changed"

```
host desktop
  ! pkg:bluez drifted: patches changed, run fspec update
```

```
Error: preparing store

Caused by:
    0: pkg bluez
    1: patches changed, run fspec update
```

**Cause.** The lock records a hash of each package's `patches = [...]` files. You edited a patch
file, or added or removed one, since the last `fspec update`, so the locked build no longer
matches the spec.

**Fix.** Run `fspec update`, then let CI or a builder build the package again (see
[Building from source](guides/source-builds.md)), and apply.

## A patch does not apply

The error chain names the patch, followed by the reason it failed:

```
applying patch patches/bluez-fix.patch
```

**Cause.** The package's new upstream version changed the lines the patch touches.

**Fix.** Rebase the patch onto the new version, or pin the old one with `version = "..."` on the
`pkg` until you have. Then run `fspec update`.

## The host is declared as another platform or distro

```
Error: host desktop is declared as arch but this machine runs fedora
```

```
Error: host laptop is declared as x86_64-linux but this machine is aarch64-linux
```

On a remote host:

```
Error: host server is declared as x86_64-linux/debian but the machine is x86_64-linux/ubuntu
```

**Cause.** fspec picks the host whose `name` equals this machine's host name, and checks that
the machine is what that host declares before it changes anything. Either the `platform` or
`system` in the spec is wrong, or fspec picked the wrong host.

**Fix.**

- If the declaration is wrong, correct `platform` or `system` in the `host(...)`.
- If the machine runs another distro on purpose, because you are moving it to the one the spec
  declares, `fspec plan` says so and names the command:

  ```
  host desktop declares arch and this machine runs fedora: `fspec switch --distro arch` installs it beside this root
  ```

  See [Switching distros](guides/switching-distros.md).
- If two machines share a host name, rename one, or pass `--host <name>` to choose.

## fspec is missing on a remote host

```
Error: fspec is not installed on server (not on PATH for a non-interactive ssh shell); install it there first, e.g. `cargo install --git https://github.com/FormalStudio/formalspec fspec`
```

**Cause.** `fspec apply --host server` runs `fspec` on the host over ssh. A non-interactive ssh
shell often has a shorter `PATH` than your login shell, so `~/.cargo/bin` is missing from it.

**Fix.** Install fspec on the host, then link it onto the default path:

```sh
ssh server sudo ln -s ~/.cargo/bin/fspec /usr/local/bin/fspec
ssh server fspec --version
```

See [Install fspec](install.md#make-it-reachable-over-ssh).

## fspec on a remote host is older

```
Error: fspec on server is older than this one (engine 2 < 3) and fspec.lock pins no fspec; run `fspec update fspec`, or install a current fspec there first
```

**Cause.** Every release that adds a builtin, an argument or a lock key raises the engine
number. A host with an older fspec would misread the spec. `fspec apply` installs the fspec the
lock pins on an older Linux host before shipping the spec (its CI build, or else a build on the
host), and stops here when the lock pins no fspec at all.

**Fix.** Run `fspec update fspec`, commit the lock, then apply again.
Or update fspec on that host the same way you installed it, for example:

```sh
ssh server cargo install --git https://github.com/FormalStudio/formalspec --locked fspec
```

## The host is unreachable over ssh

```
Error: host server is unreachable over ssh (server): ssh: connect to host server port 22: Connection refused
```

**Cause.** ssh itself failed: the host is down, the name does not resolve, or ssh asks for
something it cannot get in batch mode, such as a password or an unknown host key.

**Fix.** Check that `ssh server true` works without a prompt. If the host is reached under
another name or user, set `ssh = "alex@server.lan"` on its `host(...)`. `fspec plan --all` skips
an unreachable host and keeps going; `fspec apply` stops.

## The spec tree has uncommitted changes

```
Error: the spec tree has uncommitted changes; commit them so every host applies the same commit, or pass --dirty
```

**Cause.** A remote apply ships the spec as a git commit, so every host applies the same one.
Your working tree has edits that are not in that commit.

**Fix.** Commit the change, then apply:

```sh
git commit -am "server: add nginx"
fspec apply --host server
```

To try an edit on a host before you commit it, pass `--dirty`. fspec then ships the working
tree, untracked files included, under a name that includes a hash of its content.

## A file was changed outside fspec

```
Error: apply failed and was rolled back

Caused by:
    0: files: applying
    1: ~/.config/app/settings.conf
    2: /home/alex/.config/app/settings.conf was changed outside fspec; run `fspec adopt ~/.config/app/settings.conf` to keep the change, or set on_drift = "overwrite"
```

**Cause.** The file has `on_drift = "fail"`, and its content on disk is not what fspec last
wrote. With the default `on_drift = "overwrite"`, apply would have put the spec's content back.

**Fix.** Decide which version you keep:

- Keep the edit: for a file with `src`, run the `fspec adopt` command from the message, which
  copies the file back into the repository. For a file with `content`, copy the edit into the
  spec.
- Discard the edit: delete the file, or set `on_drift = "overwrite"` for one apply.

`fspec plan` shows the drift before you apply, with `!`.

## A patched file changed since fspec last wrote it

```
Error: preparing files

Caused by:
    0: ~/.config/app/state.json
    1: /home/alex/.config/app/state.json changed since fspec last wrote `theme` and on_conflict is "fail"
```

**Cause.** The file is in `patch` mode with `on_conflict = "fail"`. The app, or you, changed a
key the spec owns, and the spec also wants a new value for it.

**Fix.** Choose a side for that key. Set `on_conflict = "ours"` to write the spec's value, or
`"theirs"` to keep what is on disk, and apply. Or edit the key on disk back to the value fspec
last wrote. [Files](guides/files.md) explains the three-way merge.

## A path is under a protected path

```
Error: cannot apply:
  file:/home/sam/.profile: /home/sam/.profile: /home/sam/.profile is under protected path /home
```

**Cause.** The host lists that path, or one of its parents, in `protect = [...]`. fspec refuses
to write, link, remove or trash anything there. The home of the user who runs fspec stays usable
under a protected `/home`; other users' homes do not.

**Fix.** Remove the resource from the spec, or narrow `protect` if you meant fspec to manage that
path. The same check applies to app data, so a `data = [...]` path under a protected path fails
with `data of pkg:<name>` in front.

## The system scope apply failed

```
Error: system scope apply failed
```

**Cause.** `fspec apply` runs the system scope as root by starting itself again through `sudo`.
That run failed. Either sudo refused (a wrong password, an account that is not in sudoers, or no
terminal to ask on), or the system scope itself failed, and its own `Error:` is printed right
above this line. The same message appears for `plan`, `rollback` and `gc`.

**Fix.**

1. Read the error above `system scope apply failed`, if there is one, and fix that.
2. Check that sudo works in this terminal:

   ```sh
   sudo -v
   ```

3. For runs with no terminal, such as a timer, allow the fspec binary without a password. Name
   its real path, which `readlink -f "$(command -v fspec)"` prints:

   ```starlark
   file("/etc/sudoers.d/fspec", content = "alex ALL=(root) NOPASSWD: /usr/local/bin/fspec\n",
        perms = "0440")
   ```

To apply the user scope alone, pass `--scope user`.

## A service is not supported on this init system

```
host laptop
  + service:sshd  enable the distro unit
  ? service:sshd: service:sshd has no exec, and enabling a unit the system ships is not supported on launchd
```

```
Error: cannot apply:
  service:sshd: service:sshd has no exec, and enabling a unit the system ships is not supported on launchd
```

**Cause.** A `?` line in the plan is a resource that no backend can apply on this host, and
apply refuses to start while there is one. For services, the common cases are:

| Message | Means |
| --- | --- |
| `has no exec, and enabling a unit the system ships is not supported on launchd` (or `s6`) | A `service(name)` without `exec` enables a unit the distro ships, which launchd and s6 have no equivalent for |
| `sets user = True, but OpenRC has no user services` | OpenRC runs system services only |
| `sets user = True, but fspec sets up no per-user s6 supervision tree` | The same on s6 |
| `orders itself after <x>.target, a systemd target, and this host uses runit` | A literal systemd target in `after` on another init |
| `has no exec, but /etc/sv/<name> does not exist; install the package that ships it` | runit: the package with the service is not installed |

**Fix.** Give the service an `exec` so fspec writes the unit itself, make it a system service,
or declare it only on hosts whose init supports it, for example with
`if host.init == "systemd":` in a module. [Services](guides/services.md) lists what each init
supports.

## A zypper repository is unreachable and its packages wait

```
fspec: deferred repo:nvidia: refreshing it failed: Download (curl) error for 'https://download.nvidia.com/opensuse/tumbleweed/repodata/repomd.xml'; the next apply tries it again
fspec: deferred pkg:nvidia-video-G06: repo:nvidia is deferred; the next apply tries it again
```

**Cause.** On Tumbleweed, a `repo(...)` whose metadata cannot be refreshed, or whose signing key
cannot be downloaded, is deferred instead of failing the apply. fspec leaves the repository
disabled so the distro upgrade and the other repositories keep working. Packages tied to it with
`zypper(..., repo = "nvidia")`, and the services and files declared on them, wait with it.

**Fix.** Nothing, if the outage is temporary: the next apply tries again. If it persists, check
the repository URL in a browser and from the host (`curl -I <url>/repodata/repomd.xml`). A
filtering proxy that intercepts TLS causes the same symptom. A key that downloads but has a
different fingerprint than the spec pins is not deferred. It fails the apply:

```
repo:nvidia: https://download.nvidia.com/opensuse/tumbleweed/repodata/repomd.xml.key has fingerprint ..., the spec pins ...
```

## No age identity found

```
Error: preparing files

Caused by:
    0: ~/.config/app/token
    1: no age identity found (tried /home/alex/.ssh/id_ed25519, /home/alex/.ssh/id_rsa, /home/alex/.config/fspec/age.key)
```

**Cause.** A `secret(...)` needs a private key to decrypt its `.age` file, and none of the files
fspec tried exists. In the system scope the defaults are the ssh host keys,
`/etc/ssh/ssh_host_ed25519_key` and `/etc/ssh/ssh_host_rsa_key`, then `~/.config/fspec/age.key`.

**Fix.** Put a key that the file is encrypted to at one of those paths, or name yours with
`identities = [...]` on the `secret`. If the key exists but the file was not encrypted to it, add
its public key to `secrets.star` and rekey. A key with a passphrase cannot be used:
`<path> is a passphrase-protected or unsupported SSH key`. [Secrets](guides/secrets.md) covers
both.

## An apply failed and was rolled back

```
Error: apply failed and was rolled back

Caused by:
    ...
```

**Cause.** A step of the apply failed after it started changing the machine. fspec replayed its
journal backwards, so the machine and the state file are as they were before. The lines under
`Caused by:` name the step and the reason.

**Fix.** Fix the cause and apply again. If the message says
`apply failed and rollback was incomplete`, some undo steps failed too and are listed. Fix what
they name, then run `fspec apply`, which reverts any journal left behind before it starts. See
[Apply is a transaction](guides/removal-and-rollback.md#apply-is-a-transaction).

## fspec sync skips or fails an item

`fspec sync` has its own messages, such as a locked keyring, a peer that is unreachable or items
that differ between hosts. They are listed in
[Sync between hosts](guides/sync.md#troubleshooting).

## Getting help

If none of this matches, file an issue at
[github.com/FormalStudio/formalspec/issues](https://github.com/FormalStudio/formalspec/issues).
Include:

1. The command that failed and its full output.
2. The output of `fspec plan` on the same host.
3. The `host(...)` and the resource the error names, from your spec. Leave out secrets.
