+++
title = "Remote hosts"
description = "Keep several machines in one spec repository and apply or plan them over ssh from one place."
+++

One spec repository can declare every machine you own. You edit it on one machine and apply any
host from there: `fspec` copies the committed spec to the other machine over ssh and runs the
apply on it. Use this when you manage a desktop, a laptop and a server from the same repository,
or when a machine has no checkout of the spec.

Prerequisites:

- `fspec` installed on every host, reachable on the `PATH` of a non-interactive ssh shell
  ([Make it reachable over ssh](../install.md#make-it-reachable-over-ssh)).
- ssh key authentication from the machine you drive the apply from to each host. fspec probes
  and copies with `BatchMode=yes`, so a host that asks for a password counts as unreachable.
- The spec in a git repository, with `git` and `tar` on the driving machine.

## Declare several hosts

A spec lists its hosts in `hosts`. Each host is a separate machine with its own `platform`,
`system` and modules. Shared parts go in modules that more than one host loads
([Hosts and modules](../concepts/hosts-and-modules.md)).

```starlark
load("//modules/cli.star", "cli")

hosts = [
    host(
        name = "laptop",
        platform = "aarch64-darwin",
        modules = [cli],
    ),
    host(
        name = "desktop",
        platform = "x86_64-linux",
        system = fedora(packages = ["NetworkManager", "sudo"]),
        users = [user("alex", uid = 1000, shell = "fish")],
        modules = [cli],
    ),
    host(
        name = "server",
        platform = "x86_64-linux",
        system = debian(packages = ["openssh-server"]),
        ssh = "alex@server.example.net",
        users = [user("alex", uid = 1000)],
        modules = [cli],
    ),
]
```

A host is this machine when its `name` equals the system host name, lowercased, up to the first
dot. Every other host is remote. Run `fspec update` once on the machine you edit from; the lock
holds versions for every host's platform.

## Tell fspec how to reach a host

fspec reaches a remote host with `ssh <destination>`. The destination is the host's `ssh` field,
and defaults to its `name`.

The simplest setup leaves `ssh` out and puts the details in `~/.ssh/config`, so keys, ports, users
and jump hosts live where ssh already looks for them:

```text
Host desktop
    HostName 192.168.1.20
    User alex

Host server
    HostName server.example.net
    User alex
    Port 2222
```

Set `ssh` when the name ssh knows differs from the host's name, or to name the user inline:

```starlark
host(name = "server", ssh = "alex@server.example.net", ...)
host(name = "vm", ssh = "ssh://root@127.0.0.1:2222", ...)
```

The value is passed to `ssh` as one argument, so anything `ssh` accepts as a destination works,
including an alias from `~/.ssh/config` and an `ssh://` URI with a port.

## Apply a remote host

1. Commit your changes. fspec applies commits, not working trees:

   ```sh
   git commit -am "server: add ripgrep"
   ```

2. Apply the host by name:

   ```sh
   fspec apply --host server
   ```

The plan and the apply output print on your terminal as if you ran them on the host. When the
system scope needs root and you log in as a normal user, `sudo` on the host asks for that user's
password in your terminal.

`fspec plan --host server` works the same way and changes nothing on the host.

## Apply every host

```sh
fspec apply --all
```

`--all` applies the remote hosts in the order the spec lists them, then this machine last. It
stops at the first host that fails and prints which hosts converged and which it did not try:

```text
Error: host desktop failed: <the error>
converged: server
not attempted: laptop
```

Fix the failure and run `fspec apply --all` again. Hosts that already converged have nothing to
do and finish quickly.

## Plan every host

```sh
fspec plan --all
```

`plan --all` prints one plan per host. A remote host that does not answer over ssh is skipped
instead of failing the run, since its state lives on that machine:

```text
host server
  unreachable, skipped
```

A host that answers but cannot be planned prints `error:` with the reason under its name. The
command exits with `<n> host(s) could not be planned` when any host failed.

## Apply uncommitted changes

To try a change on a remote host before you commit it, pass `--dirty`:

```sh
fspec apply --host server --dirty
```

Use it for experiments. Two hosts that apply the same commit end up with the same software; a
dirty apply breaks that link, so commit once the change works.

## How it works

`fspec apply --host server` on a remote host runs four steps. `plan` runs the same steps with
`plan --json` at the end and renders the result on your machine.

### 1. Probe

```sh
ssh -o ConnectTimeout=5 -o BatchMode=yes server 'PATH="/usr/local/bin:$PATH" fspec facts --json'
```

The host prints what it is:

```json
{"hostname":"server","platform":"x86_64-linux","distro":"debian","engine":3}
```

fspec compares the platform and distro with the declaration and stops before anything is copied
when they differ. The probe is only a check. It never feeds into evaluation, so the spec decides
what a host is, and a wrong `ssh` destination cannot turn into a wrong apply.

`engine` is a number that goes up whenever fspec learns a new builtin, builtin argument, resource
field or lock key. An fspec on the host with a lower number may reject the spec or ignore lock
entries it does not know, so fspec first installs the fspec your lock pins there: the CI build
when the lock records one for the host's platform, otherwise a build from the locked commit,
run by the host's builders or, when it names none, by the host's own fspec (minutes, the first
time). fspec never copies itself to the host, since the
binary on your machine may not run there.

### 2. Check the tree

fspec runs `git status --porcelain` in the spec directory. Any uncommitted change, untracked files
included, stops the apply unless you pass `--dirty`.

### 3. Ship the spec

fspec archives the files `git ls-files` lists in the spec directory, plus `fspec.lock`, and
unpacks them on the host into:

```text
~/.cache/fspec/spec/<commit>/
```

With `--dirty`, the archive also holds untracked files that git does not ignore, and lands in
`~/.cache/fspec/spec/<commit>-dirty-<hash>/`. The hash covers every shipped file, so a second edit
at the same commit gets its own directory. Earlier dirty copies of that commit are deleted first.

Files loaded with `gh:` ([Hosts and modules](../concepts/hosts-and-modules.md)) are copied from
this machine's load cache, `~/.cache/fspec/loads`, into `.fspec/loads` inside the shipped
directory. The host evaluates them without fetching anything, so it needs no GitHub token, even
for a private repository.

Only the spec travels. Packages, secrets and files are fetched, decrypted and written on the host
by the host's own fspec, from the lock.

### 4. Run the apply

```sh
ssh -t server 'PATH="/usr/local/bin:$PATH" fspec --spec "$HOME/.cache/fspec/spec/<commit>"/spec.star apply --local --host server'
```

`-t` hands your terminal to the remote command, so `sudo` there can prompt. `--local` tells the
remote fspec to apply on the machine it runs on, even when that machine's host name differs from
the declared one, instead of trying to forward the apply again.

`--scope system` or `--scope user` on your command is passed through.

### Under --root

A run with `--root` never goes remote: every host is treated as a local dry run.
`fspec apply --all` refuses under `--root`.

## Troubleshooting

**`host server is unreachable over ssh (server): ...`**

ssh exited 255: it could not connect or log in. Check `ssh -o BatchMode=yes server true` by hand.
A host that asks for a password fails here; set up key authentication.

**`fspec is not installed on server (not on PATH for a non-interactive ssh shell); install it there first, ...`**

The login worked and the shell found no `fspec`. Install it on the host, or link it into a
directory on the default path such as `/usr/local/bin`.

**`fspec on server is older than this one (engine 2 < 3) and fspec.lock pins no fspec; ...`**

An apply hands an older Linux host the fspec the lock pins before it ships the spec, and this
lock pins none. Run `fspec update fspec`, or update fspec on the host by hand.

**`host server is declared as x86_64-linux/debian but the machine is x86_64-linux/fedora`**

The `ssh` destination reaches a different machine than the declaration describes, or the host's
`system` is wrong. Fix whichever is out of date.

**`the spec tree has uncommitted changes; commit them so every host applies the same commit, or pass --dirty`**

Commit, or pass `--dirty` for a one-off test.

**`the spec must live in a git repository to be shipped`**

Run `git init` in the spec directory and commit the spec and `fspec.lock`.

**`--all applies real machines; it cannot run under --root`**

Drop `--root`, or name one host with `--host`.
