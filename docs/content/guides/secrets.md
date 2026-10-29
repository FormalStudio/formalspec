+++
title = "Secrets"
description = "Keep age-encrypted secrets in the spec repository and decrypt them onto each host at apply and at boot."
+++

A secret is a file you commit to the spec repository encrypted with [age](https://age-encryption.org),
and that fspec decrypts onto the host at apply. The repository only ever holds ciphertext, so you
can push it to a public remote. The workflow is the one agenix uses: a `secrets.star` file lists
who can decrypt each secret, `fspec secret edit` changes one, and `secret(...)` in the spec says
where its plaintext goes.

Each secret is encrypted to a set of recipients: public keys. Any matching private key, called an
identity, decrypts it. A recipient can be an age key (`age1...`) or an ssh public key
(`ssh-ed25519 ...` or `ssh-rsa ...`), so the ssh keys your machines already have work as is.

## Set up secrets

This tutorial adds an API token to a host called `laptop`, readable by the user `alex`.

1. Pick the key you decrypt with on the machine you edit from. Either use your ssh key, if it is
   ed25519 or RSA and has no passphrase:

   ```sh
   cat ~/.ssh/id_ed25519.pub
   ```

   or make an age key with `age-keygen` (from the `age` package) and print its public key:

   ```sh
   mkdir -p ~/.config/fspec
   age-keygen -o ~/.config/fspec/age.key
   age-keygen -y ~/.config/fspec/age.key
   ```

   `~/.config/fspec/age.key` is one of the paths fspec tries by default, so you never pass it.

2. Find the host's own key. For a secret in the system scope, fspec decrypts with the host's ssh
   host key by default. Print its public half on the host:

   ```sh
   cat /etc/ssh/ssh_host_ed25519_key.pub
   ```

   For a secret under `~/`, the default identities are your own keys on that host instead.

3. Create `secrets.star` next to `spec.star`. It maps each `.age` file, relative to the spec
   directory, to the public keys it is encrypted to:

   ```starlark
   alex = "age1qz5k...your age public key"
   laptop = "ssh-ed25519 AAAAC3Nza... root@laptop"

   secrets = {
       "secrets/api-token.age": [alex, laptop],
   }
   ```

   `secrets.star` has only Starlark itself, `struct` and `print`: no `load` and no other
   builtins. Loops and comprehensions work, so one list of names can cover many files.

4. Write the secret. `fspec secret edit` opens `$EDITOR` (or `vi`) on the decrypted text, and
   encrypts what you save to the file's recipients. A file that does not exist yet starts empty:

   ```console
   $ fspec secret edit api-token
   wrote secrets/api-token.age
   ```

   You can name the entry as written in `secrets.star`, by its file name, or by its file name
   without `.age`.

5. Declare where the plaintext goes, in the host's modules:

   ```starlark
   secret("/run/fspec/secrets/api-token", src = "secrets/api-token.age", owner = "alex"),
   ```

6. Commit the spec, `secrets.star` and the encrypted file:

   ```sh
   git add secrets.star secrets/api-token.age spec.star
   git commit -m "laptop: api token"
   ```

7. Apply:

   ```console
   $ fspec apply
   host laptop
     + secret:/run/fspec/secrets/api-token  secrets/api-token.age
     1 to add, 0 to change, 0 to remove
   ```

   fspec decrypts the file with the host key, writes it with mode `0400`, and runs
   `chown alex` on it.

Programs then read the secret from its path, for example
`export OPENAI_API_KEY="$(cat /run/fspec/secrets/api-token)"` in a shell rc file.

## Change a secret

Run `fspec secret edit` again, then commit and apply. The plan shows the change as
`source changed`:

```console
$ fspec secret edit api-token
wrote secrets/api-token.age
$ fspec plan
host laptop
  ! secret:/run/fspec/secrets/api-token drifted: source changed
  0 to add, 1 to change, 0 to remove
```

`fspec secret show api-token` prints the plaintext, and `fspec secret list` shows every entry
with its recipients and whether its file exists yet.

Closing the editor without a change prints `secrets/api-token.age unchanged` and leaves the
file alone.

## Add or remove a recipient

To give a new machine access, or take it away, change the lists in `secrets.star` and re-encrypt
every file to its new recipients:

1. Add the new host's public key to `secrets.star`, or remove a key.
2. Re-encrypt:

   ```console
   $ fspec secret rekey
   rekeyed secrets/api-token.age
   ```

   An entry whose `.age` file does not exist yet is skipped with `skipped <file>: it does not
   exist`.

3. Commit and apply. Each host decrypts the re-encrypted file again.

`rekey` must decrypt every file first, so your identity has to be a recipient of all of them.
Removing a recipient stops it from decrypting new versions. It does not undo what that key could
read before, so rotate the secret itself (`fspec secret edit`) when a key is compromised.

## Pick the identities

`identities` lists the private keys a host tries, in order. Missing files are skipped. Without it,
fspec uses the defaults for the secret's scope:

| Scope | Default identities |
| --- | --- |
| System (absolute `path`) | `/etc/ssh/ssh_host_ed25519_key`, `/etc/ssh/ssh_host_rsa_key`, `~/.config/fspec/age.key`, `~/.config/age/keys.txt` |
| User (`~/` path) | `~/.ssh/id_ed25519`, `~/.ssh/id_rsa`, `~/.config/fspec/age.key`, `~/.config/age/keys.txt` |

`~` is the home of the user running fspec. State records the identity paths as absolute paths, so
`fspec secret install` at boot uses the same files. An identity is an unencrypted ed25519 or RSA ssh key,
or an age identity file. fspec decrypts without asking for a passphrase, because it also runs at
boot.

Set `identities` when your secrets are encrypted to a key at another path:

```starlark
secret(
    "/run/fspec/secrets/api-token",
    src = "secrets/api-token.age",
    identities = ["/etc/ssh/ssh_host_ed25519_key", "~/.config/age/keys.txt"],
)
```

`fspec secret edit`, `show` and `rekey` use the user-scope defaults. Pass `-i <path>` (or
`--identity`, repeatable) to use other keys.

## A machine without a key yet

By default an apply stops before changing anything when a secret does not decrypt. On a machine
that has no key yet, such as a fresh install, `fspec apply --skip-undecryptable-secrets` applies
everything else, prints `warning: skipped secret <path>: <why>` for each secret it left out, and
lists them at the end. A secret an earlier apply installed is never left out, since leaving it
out would remove it. Put a key in place, or have one added to `secrets.star` and
`fspec secret rekey` run on a machine that already decrypts, then run `fspec apply` again.

The [installer](installer.md#keys-for-your-secrets) can bring a key in, or make one, and runs its
first apply this way.

## Owner and permissions

| Argument | Default | Meaning |
| --- | --- | --- |
| `perms` | `"0400"` | Octal permission bits |
| `owner` | none | `user` or `user:group`, applied with `chown` after the file is written |

The file is created with mode `0600` and renamed into place, so the plaintext is never readable
by anyone else, even for a moment. A system-scope secret without `owner` belongs to root.

A user-scope secret, such as `secret("~/.config/app/token", src = ..., perms = "0600")`, is
written as you and needs no `owner`.

## Keep secrets off the disk with /run

`/run` on Linux is a tmpfs, so plaintext under it never reaches the disk. macOS empties `/var/run`
at boot. Either way the secret is gone after a reboot until the next apply.

`fspec secret install` closes that gap. At apply, fspec keeps a copy of each secret's
ciphertext in its state directory. `fspec secret install` decrypts every secret of the last apply
again from those copies, with the same identities, without reading the spec. Run it at boot as a
oneshot service:

```starlark
SECRETS = ["api-token", "db-password"]

def secrets(host):
    return [
        secret("/run/fspec/secrets/" + n, src = "secrets/%s.age" % n, owner = "alex")
        for n in SECRETS
    ] + [
        service(
            "secrets",
            exec = "/usr/local/bin/fspec secret install",
            description = "Decrypt fspec's secrets into /run/fspec/secrets",
            systemd = {
                "Type": "oneshot",
                "RemainAfterExit": True,
                "unit": {"Before": "systemd-user-sessions.service"},
            },
        ),
    ]
```

On systemd this is `fspec-secrets.service`. `Type=oneshot` with `RemainAfterExit` makes it run
once and count as active afterwards, and `Before=systemd-user-sessions.service` finishes it before
anyone can log in. On macOS, drop the `systemd` argument: a launchd daemon runs once at load by
default.

```starlark
service("secrets", exec = "/usr/local/bin/fspec secret install"),
```

`exec` needs the full path of an `fspec` that root can run; see
[Install fspec](../install.md#make-it-reachable-over-ssh) for putting it in `/usr/local/bin`.
`fspec secret install` handles the system scope by default; `--scope user` decrypts the user
scope's secrets. It prints each path it wrote.

## What fspec stores

| What | Where | Contains |
| --- | --- | --- |
| The `.age` file | The spec repository | Ciphertext only |
| State entry | `state.json` of the secret's scope | The `secret(...)` arguments, the sha256 of the plaintext and of the ciphertext, and the identity paths, made absolute |
| Kept ciphertext | `secrets/<sha256>.age` in the scope's state directory, mode `0600` | A copy of the `.age` file, for `fspec secret install` |
| Plaintext | The secret's `path` | The decrypted file |

The plaintext is never in state, in a generation, or in the staging directory after apply.
[Paths](../reference/paths.md) lists the state directories.

## Drift and removal

A secret drifts like a [managed file](files.md#drift): `content changed` when the plaintext on
disk was edited, `file is missing` when it was deleted, `source changed` when the `.age` file in
the repository changed. The next apply writes it again. `fspec watch` reports secrets too.

Removing a `secret` line deletes the plaintext on the next apply, as long as it still matches what
fspec wrote.

## Troubleshooting

| Error | Fix |
| --- | --- |
| ``no age identity found (tried /etc/ssh/ssh_host_ed25519_key, /etc/ssh/ssh_host_rsa_key, /root/.config/fspec/age.key)`` | None of the identity files exists on this host. Copy a key there or set `identities`. `--skip-undecryptable-secrets` applies the rest meanwhile |
| ``/home/alex/.ssh/id_ed25519 is a passphrase-protected or unsupported SSH key`` | fspec cannot ask for a passphrase. Use an age key, or another ssh key without one |
| ``decrypting`` followed by ``No matching keys found`` | The identities exist, but none is a recipient of this file. Add the host's key to `secrets.star` and run `fspec secret rekey` |
| ``api-token is not in secrets.star`` | `fspec secret edit` only edits entries listed in `secrets.star` |
| ``api-token names more than one entry in secrets.star`` | Two entries share that file name. Pass the full path as written in `secrets.star` |
| ``/home/alex/spec/secrets.star must set `secrets` `` | `secrets.star` has to define a dict named `secrets` |
| ``<path>: the kept ciphertext is missing`` | `fspec secret install` found a secret in state without its kept copy. Run `fspec apply` once |
