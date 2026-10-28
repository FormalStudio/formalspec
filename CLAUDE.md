# CLAUDE.md

> fspec is basically nix if it were faster, not centralized, and compatible with any system and its package manager.

Judge every design choice, engine feature and spec pattern against that line. A change that
pulls fspec back toward how nix works for nix's own reasons is the wrong change.

- **Faster.** No full-world evaluation and no rebuilds of things a binary already exists for.
  Distro packages and upstream binaries come first; a source build is the last resort.
- **Not centralized.** There is no single nixpkgs. Any repo can ship its own `.star`, and
  recipes live with the user's spec or with the upstream project, never in a central tree.
- **Any system.** fspec runs on top of the host OS and drives its native package manager
  (zypper, pacman, dnf, apt, brew) instead of replacing it.

`docs/design.md` is the design. `docs/from-nix.md` maps nix constructs to their fspec shape;
add each mapping you settle there, and drop nix-isms that only exist to work around nix.

## What fspec is not

- Not a package collection. It installs what the distro, Flatpak, Homebrew or an upstream
  release already ships, and builds from source only for software none of them has.
- Not a derivation model. A source build is one `git(...)` package with its own recipe, built by
  CI, a builder or the host, never a graph that rebuilds the world when one input moves.
- Not a replacement OS. It does not own `/usr`; the host's package manager does.

## Crates

| Crate | Owns |
|---|---|
| `fspec` | The CLI: argument parsing, `Env`, the sudo handoff of the system scope |
| `fspec-core` | Resources, `Cx`, plan and apply, the state file, the lock, the `Runner` |
| `fspec-eval` | Starlark builtins (`host`, `pkg`, `file`, `service`, ...) into resources |
| `fspec-sources` | `fspec update`: resolving packages into the lock |
| `fspec-store` | Store packages under `/opt/fspec`, generations, links |
| `fspec-build` | `git(...)` builds: recipes, packing, builders over ssh, the CI cache |
| `fspec-files` | `file` and `secret` |
| `fspec-os` | Services per init, users, macOS defaults, distro units |
| `fspec-pkgmgr` | Native package managers, Flatpak, Homebrew |
| `fspec-image` | bootc images and their CI |
| `fspec-remote` | Shipping the spec and applying or planning over ssh |
| `fspec-sync` | `fspec sync`: app data between the user's hosts over ssh |
| `fspec-switch` | `fspec switch`, `forget`, `disk`: pure functions returning command plans |
| `fspec-try` | `fspec try`: distro ISOs, the qemu command and the script that runs a throwaway VM |
| `fspec-install` | The installer |
| `fspec-dev` | Project environments from `dev.star` |

## Build and test

- `cargo build`, `cargo test --workspace`. CI also runs `cargo fmt --all --check` and
  `cargo clippy --workspace --all-targets -- -D warnings`; run all four before pushing.
- Every external command goes through a `Runner`; tests use `RecordingRunner` and temp roots.
  Do not test against the real machine from `cargo test`.
- The real spec for Kyan's hosts is the `main` branch of FormalSnake/dotfiles, checked out at
  `~/.config/formalspec`, not in this repo. Its old nix config is that repo's `nix` branch.

## Conventions

- Docs must match the code. A change to behaviour, a builtin, a flag or a resource field updates
  `docs/design.md` (and `docs/from-nix.md` where it maps a nix construct) in the same commit,
  and the user docs in `docs/content/` (the os.formalsnake.dev/docs site): the guide that covers
  it, `reference/builtins.md` for a builtin or field, `reference/commands.md` for a flag.
- Comments state constraints the code cannot show. No em dashes in code, docs or commits.
- Commits are short lowercase conventional subjects (`feat(build): ...`), one unit of work each.
- A new builtin, builtin argument, resource field or lock key bumps `fspec_remote::ENGINE`, so a
  remote host running an older fspec is refused instead of misreading the spec.
- Prefer a plan function that returns commands over code that runs them; it keeps the step
  testable and printable.
