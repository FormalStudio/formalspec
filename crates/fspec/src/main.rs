mod backends;
mod dev;
mod secret;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use fspec_core::apply::{apply_scope, desired_from_generation, plan_scope, state_dir};
use fspec_core::engine;
use fspec_core::facts::{Facts, local_hostname};
use fspec_core::fetch::LiveFetch;
use fspec_core::resource::{Pkg, Source, System};
use fspec_core::runner::{Cmd, LiveRunner, LogRunner, Runner};
use fspec_core::{Cx, Host, Lock, Paths, Plan, Resource, Scope, State};
use fspec_remote::{Planned, Probe, Remote, Target};

#[derive(Parser)]
#[command(version, about = "Declare your machines in Starlark and keep them identical")]
struct Cli {
    /// The spec entry point, or a directory holding spec.star. Without it: a spec.star in this
    /// directory, then (plan and update) the dev.star project around it, then $FSPEC_SPEC.
    #[arg(short, long, global = true)]
    spec: Option<PathBuf>,
    /// Treat this directory as `/`. Everything fspec reads or writes stays under it.
    #[arg(long, global = true, default_value = "/")]
    root: PathBuf,
    /// The user's home, default $HOME (with --root, `<root>/home/<user>`).
    #[arg(long, global = true)]
    home: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show what apply would change on this host.
    Plan {
        /// Host to plan, default this machine's host name.
        #[arg(long)]
        host: Option<String>,
        /// Print the plan as JSON.
        #[arg(long, conflicts_with = "all")]
        json: bool,
        /// Plan every host in the spec, asking each remote host over ssh.
        #[arg(long, conflicts_with = "host")]
        all: bool,
        /// Ship a spec tree with uncommitted changes to remote hosts anyway.
        #[arg(long)]
        dirty: bool,
        /// Plan on this machine whatever the host is called. Set on the remote end of an ssh plan.
        #[arg(long, hide = true)]
        local: bool,
        /// Plan one scope only; a non-root plan hands the system scope to sudo with it.
        #[arg(long, hide = true)]
        scope: Option<ScopeArg>,
    },
    /// Converge a host, over ssh when it is not this machine.
    Apply {
        #[arg(long)]
        host: Option<String>,
        /// Apply every host in the spec, remote ones first and this machine last; stops at the first failure.
        #[arg(long, conflicts_with = "host")]
        all: bool,
        /// Ship a spec tree with uncommitted changes to remote hosts anyway.
        #[arg(long)]
        dirty: bool,
        /// Apply on this machine whatever the host is called. Set on the remote end of an ssh apply.
        #[arg(long, hide = true)]
        local: bool,
        /// Apply one scope only. Without it, the system scope runs (through sudo when needed),
        /// then the user scope.
        #[arg(long)]
        scope: Option<ScopeArg>,
    },
    /// Resolve versions and hashes into fspec.lock, next to the spec.
    Update {
        /// Only re-resolve these packages, default every package.
        pkgs: Vec<String>,
        /// Pin each image host's pushed image digest instead of resolving packages.
        #[arg(long, conflicts_with_all = ["pkgs", "cache"])]
        images: bool,
        /// Also record the digest of each CI-built artifact (see `fspec ci packages`).
        #[arg(long)]
        cache: bool,
        /// Registry for --images (default `ghcr.io/<origin owner>`) or --cache (default
        /// `ghcr.io/<origin owner>/fspec-cache`).
        #[arg(long)]
        registry: Option<String>,
    },
    /// Build the bootc image of a host.
    Image {
        #[command(subcommand)]
        command: ImageCommand,
    },
    /// Build a `git(...)` package from source at its locked commit into a `.tar.zst`.
    Build {
        #[arg(long, required_unless_present = "job")]
        pkg: Option<String>,
        /// A job file a host shipped to this builder; its patches sit beside it.
        #[arg(long, hide = true, conflicts_with = "pkg")]
        job: Option<PathBuf>,
        /// Must be this machine's platform, e.g. `x86_64-linux`.
        #[arg(long)]
        platform: String,
        #[arg(long)]
        out: PathBuf,
        /// Build on this host's builders, falling back to this machine.
        #[arg(long, conflicts_with = "job")]
        host: Option<String>,
    },
    /// Generate CI that builds image hosts and packages.
    Ci {
        #[command(subcommand)]
        command: CiCommand,
    },
    /// Pull an app's edit of a managed file back into the repo.
    Adopt {
        /// The file as written in the spec (`~/.config/x`) or its path on disk.
        path: String,
        #[arg(long)]
        host: Option<String>,
    },
    /// Report drift on managed files as it happens.
    Watch,
    /// Re-apply an earlier generation as a new one. Store packages come back by moving
    /// `current`, since their old store paths are still there.
    Rollback {
        /// The generation to restore, default the one before the current.
        generation: Option<u64>,
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        scope: Option<ScopeArg>,
    },
    /// Install another distro beside this one, or reset a bootc host's /etc and /var.
    Switch {
        /// Bootstrap this distro into a new btrfs root subvolume. The host's `system` must already
        /// declare it.
        #[arg(long, value_parser = fspec_switch::DISTROS, conflicts_with_all = ["reset_var", "to_image", "adopt_boot"])]
        distro: Option<String>,
        /// Make this root own boot: its entry becomes the default, or under GRUB it installs its
        /// own GRUB with the previous owner as a fallback. `forget` needs it.
        #[arg(long, conflicts_with_all = ["reset_var", "to_image"])]
        adopt_boot: bool,
        /// Stage a reset of /etc and /var to the staged image's defaults for the next boot, keeping
        /// the host's `persist` paths.
        #[arg(long, conflicts_with = "to_image")]
        reset_var: bool,
        /// Convert this classic system to the bootc image the host declares (destructive).
        #[arg(long)]
        to_image: bool,
        /// Install into a new subvolume @<NAME> beside the other roots, booted by the systemd-boot
        /// already on the ESP: the host's image with --to-image, else the host's tumbleweed() at
        /// the snapshot fspec.lock pins. Nothing else on the disk changes.
        #[arg(long, value_name = "NAME")]
        beside: Option<String>,
        /// With --beside, the subvolume whose ssh host keys, network connections, tailscale state
        /// and users the new root keeps, default the running root.
        #[arg(long, value_name = "SUBVOLUME", requires = "beside")]
        inherit: Option<String>,
        /// With --beside, pull the locked digest from this plain-HTTP registry repository instead.
        /// The new root still updates from the locked repository's `:latest`.
        #[arg(long, value_name = "REPOSITORY", requires_all = ["beside", "to_image"])]
        pull_from: Option<String>,
        /// With --beside and no --to-image, the btrfs partition to install on when this system
        /// does not run from it, as from a live system. --inherit then names the root to take the
        /// identity from.
        #[arg(long, value_name = "DEVICE", requires = "inherit", conflicts_with = "to_image")]
        device: Option<String>,
        #[arg(long)]
        host: Option<String>,
        /// Fedora release number or Debian suite to bootstrap.
        #[arg(long, requires = "distro")]
        release: Option<String>,
        /// Confirm --to-image, which reinitialises /boot.
        #[arg(long)]
        yes: bool,
    },
    /// Delete a distro root created by `fspec switch --distro` or `--beside`, or the NixOS a
    /// migration kept in @nixos, and its boot entries.
    Forget { distro: String },
    /// agenix's workflow for the `.age` files listed in `secrets.star` next to the spec.
    Secret {
        #[command(subcommand)]
        cmd: SecretCommand,
    },
    /// Put a disk onto a layout that `switch --distro` installs roots side by side on.
    Disk {
        #[command(subcommand)]
        cmd: DiskCommand,
    },
    /// Print what this machine is, as `apply` compares it with the declaration.
    /// Sync app data with the other hosts that list the same `sync(...)` item.
    Sync {
        /// Items to sync, default every item this host lists.
        items: Vec<String>,
        /// Only with this peer.
        #[arg(long)]
        peer: Option<String>,
        /// Copy each item from this host over the other's copy: the handoff of a `seed` item.
        #[arg(long)]
        from: Option<String>,
        /// Put the items' previous version back on this host.
        #[arg(long, conflicts_with_all = ["from", "watch"])]
        rollback: bool,
        /// Print what would move, change nothing.
        #[arg(long)]
        dry_run: bool,
        /// Sync again every INTERVAL seconds until stopped.
        #[arg(long, value_name = "INTERVAL", num_args = 0..=1, default_missing_value = "60")]
        watch: Option<u64>,
    },
    /// One request of a peer's `fspec sync`, on stdin.
    #[command(hide = true)]
    SyncAgent { op: String, item: Option<String> },
    #[command(hide = true)]
    Facts {
        #[arg(long)]
        json: bool,
    },
    /// List app data under ~/.config, ~/.local/share, ~/.local/state and ~/.cache that no
    /// package claims and no managed file covers.
    Orphans {
        /// Record PATH as PKG's data, so removing PKG trashes it.
        #[arg(long, num_args = 2, value_names = ["PATH", "PKG"], conflicts_with = "delete")]
        claim: Option<Vec<String>>,
        /// Move PATH to the trash.
        #[arg(long, value_name = "PATH")]
        delete: Option<String>,
        #[arg(long)]
        host: Option<String>,
    },
    /// Delete old generations, their snapshots, trash and store paths nothing kept references.
    Gc {
        /// Only things last touched longer ago than this: `14d`, `12h`, `30m`, `90s`, `2w`.
        #[arg(long)]
        older_than: String,
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        scope: Option<ScopeArg>,
    },
    /// Open a shell in the project environment of the nearest dev.star, or run `-- <command>` in it.
    Dev {
        /// The project directory, default the current one.
        #[arg(long, global = true)]
        dir: Option<PathBuf>,
        #[command(subcommand)]
        cmd: Option<dev::DevCommand>,
        #[arg(last = true)]
        args: Vec<String>,
    },
}

#[derive(Subcommand)]
enum ImageCommand {
    /// Print the Containerfile CI builds for a host.
    Containerfile {
        #[arg(long)]
        host: Option<String>,
    },
}

#[derive(Subcommand)]
enum CiCommand {
    /// Write the workflow that builds and pushes every host's image.
    Init {
        /// Image registry namespace, default `ghcr.io/<owner of the git remote>`.
        #[arg(long)]
        registry: Option<String>,
    },
    /// Write .github/workflows/fspec-packages.yml: build every git package per platform and push
    /// it to the OCI cache.
    Packages,
}

#[derive(Subcommand)]
enum SecretCommand {
    /// Decrypt FILE (`openai`, `openai.age` or its path in secrets.star) into $EDITOR, then
    /// encrypt the result to its recipients. A FILE that does not exist yet starts empty.
    Edit {
        file: String,
        /// Identity to decrypt with, repeatable. Default ~/.ssh/id_ed25519, ~/.ssh/id_rsa and
        /// ~/.config/fspec/age.key.
        #[arg(short, long)]
        identity: Vec<PathBuf>,
    },
    /// Re-encrypt every file in secrets.star to its current recipients.
    Rekey {
        #[arg(short, long)]
        identity: Vec<PathBuf>,
    },
    /// Decrypt the secrets of the last apply again from their kept ciphertext. Run at boot, when
    /// the tmpfs they live on starts empty.
    Install {
        #[arg(long, value_enum, default_value = "system")]
        scope: ScopeArg,
    },
}

#[derive(Subcommand)]
enum DiskCommand {
    /// Show the steps that bring this disk, or the unmounted ext4 root partition DEVICE, onto a
    /// supported layout.
    Plan {
        device: Option<String>,
        /// For a NixOS root on DEVICE: the system to boot from @nixos, in that root's /nix/store.
        #[arg(long, requires = "device")]
        nixos_system: Option<String>,
    },
    /// Run those steps: in place on the running system, or converting DEVICE to btrfs.
    Migrate {
        device: Option<String>,
        /// For a NixOS root on DEVICE, or the running one with --stage: the system to boot from
        /// @nixos, in that root's /nix/store.
        #[arg(long)]
        nixos_system: Option<String>,
        /// From the running NixOS root on ext4: stage this live ISO to boot once from the disk,
        /// where it converts this root unattended and reboots into NixOS from @nixos.
        #[arg(long, value_name = "ISO", conflicts_with = "device", requires = "nixos_system")]
        stage: Option<PathBuf>,
        /// With --stage: an uncompressed tarball unpacked over the live system (network, ssh keys,
        /// /etc/fspec-live/guard.conf).
        #[arg(long, requires = "stage")]
        overlay: Option<PathBuf>,
        /// A backup taken in the last 24 hours, when there is no recent snapshot under @snapshots.
        #[arg(long)]
        backup: Option<PathBuf>,
        /// Convert without a backup: nothing on DEVICE needs saving. The rollback journal still applies.
        #[arg(long, conflicts_with = "backup")]
        no_backup: bool,
        /// Size of the swapfile in @swap, in GiB.
        #[arg(long, default_value_t = 4)]
        swap: u64,
    },
    /// After the first good boot of a NixOS generation a migration made, drop the trial arguments
    /// (the emergency and rescue masks, panic on an initrd failure) from its blessed boot entry.
    Unmask {
        /// Where the running system mounts the ESP.
        #[arg(long, default_value = "/boot")]
        esp: String,
    },
    /// Partition and format DEVICE from scratch.
    Init {
        device: String,
        #[arg(long, value_enum, default_value = "default")]
        layout: LayoutArg,
        /// Put the btrfs filesystem in a LUKS2 container.
        #[arg(long)]
        luks: bool,
        /// Install this distro as the first root; the host's `system` must declare it.
        #[arg(long, value_parser = fspec_switch::DISTROS, requires = "host")]
        distro: Option<String>,
        #[arg(long)]
        host: Option<String>,
        /// Fedora release number, or Debian or Ubuntu suite.
        #[arg(long, requires = "distro")]
        release: Option<String>,
        /// A backup of the disk's contents taken in the last 24 hours; not needed for a blank disk.
        #[arg(long)]
        backup: Option<PathBuf>,
        /// Size of the swapfile in @swap, in GiB.
        #[arg(long, default_value_t = 4)]
        swap: u64,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum LayoutArg {
    Default,
    Zfs,
    Bios,
}

#[derive(Clone, Copy, ValueEnum)]
enum ScopeArg {
    System,
    User,
}

impl From<ScopeArg> for Scope {
    fn from(s: ScopeArg) -> Scope {
        match s {
            ScopeArg::System => Scope::System,
            ScopeArg::User => Scope::User,
        }
    }
}

const SCOPES: [Scope; 2] = [Scope::System, Scope::User];

fn main() -> Result<()> {
    let cli = Cli::parse();
    let paths = paths(&cli)?;
    let for_project = matches!(cli.command, Command::Plan { .. } | Command::Update { .. });
    let env = std::env::var_os("FSPEC_SPEC").map(PathBuf::from);
    let picked = dev::pick_spec(cli.spec.clone(), env, &std::env::current_dir()?, for_project);
    let spec_path = std::path::absolute(picked.as_deref().unwrap_or(Path::new("spec.star")))?;
    let spec_dir = spec_path.parent().context("spec has no parent directory")?.to_path_buf();
    match &cli.command {
        Command::Plan { .. } | Command::Update { .. } if picked.is_none() => match &cli.command {
            Command::Update { pkgs, .. } => dev::update(&paths, pkgs),
            _ => dev::plan_here(&paths),
        },
        Command::Plan { host, json, all, dirty, local, scope } => {
            let spec = fspec_eval::evaluate(&spec_path)?;
            if *all {
                return plan_all(&spec, &paths, &spec_path, *dirty);
            }
            let host = named(&spec, host.as_deref())?;
            let alias = remote_alias(host, &local_hostname(), &paths, *local);
            let here = alias.is_none();
            if let Some(System::Distro(d)) = &host.system
                && here
                && !*json
                && is_live(&paths)
            {
                let running = Facts::probe_local().1;
                if d.distro != running {
                    println!(
                        "host {} declares {} and this machine runs {running}: `fspec switch --distro {}` installs it \
                         beside this root",
                        host.name, d.distro, d.distro
                    );
                    if let Some(line) = boot_line(&paths, host) {
                        println!("{line}");
                    }
                    return Ok(());
                }
            }
            let plan = match alias {
                Some(alias) => {
                    let remote = Remote {
                        runner: &LiveRunner,
                        spec_dir: spec_dir.clone(),
                        spec_file: file_name(&spec_path)?,
                        dirty: *dirty,
                        loads_cache: None,
                    };
                    match remote.plan(host, &alias)? {
                        Planned::Plan(plan) => plan,
                        Planned::Unreachable => bail!("host {} is unreachable over ssh ({alias})", host.name),
                    }
                }
                None => plan_local(&paths, &spec_path, host, *scope)?,
            };
            if *json {
                println!("{}", serde_json::to_string_pretty(&plan)?);
            } else {
                println!("{plan}");
                if let Some(line) = boot_line(&paths, host).filter(|_| here) {
                    println!("{line}");
                }
            }
            Ok(())
        }
        Command::Update { pkgs, images, cache, registry } => {
            let lock_path = spec_dir.join("fspec.lock");
            let old = Lock::load(&lock_path)?;
            let (fetch, runner) = (LiveFetch::new(), LiveRunner);
            // Updating named packages keeps every gh: load at its locked commit.
            let keep = !pkgs.is_empty() || *images;
            let spec = fspec_eval::evaluate_resolving(&spec_path, &runner, &fetch, &old.loads, keep)?;
            if *images {
                let registry = registry_for(registry.as_deref(), &runner, &spec_dir)?;
                let u = fspec_image::update_images(&spec.hosts, &old, &registry, &fetch)?;
                if u.lock != old {
                    u.lock.save(&lock_path)?;
                }
                for line in &u.changes {
                    println!("{line}");
                }
                if !u.errors.is_empty() {
                    bail!("{} image(s) failed:\n{}", u.errors.len(), u.errors.join("\n"));
                }
                return Ok(());
            }
            let sources = fspec_sources::Cx::new(&fetch, &runner).with_spec_dir(&spec_dir);
            let mut result = fspec_sources::update::update(&spec.hosts, &old, pkgs, &sources)?;
            let mut errors = std::mem::take(&mut result.errors);
            for line in &result.changes {
                println!("{line}");
            }
            for (key, load) in &spec.loads {
                match old.loads.get(key) {
                    Some(was) if was.rev == load.rev => {}
                    Some(was) => println!("{key} {} -> {}", &was.rev[..12], &load.rev[..12]),
                    None => println!("+ {key} {}", &load.rev[..12]),
                }
            }
            result.lock.loads = spec.loads.clone();
            if *cache {
                let registry = fspec_build::cache::registry(&runner, &spec_dir, registry.as_deref())?;
                let (lines, failed) = fspec_build::cache::record(&mut result.lock, &spec.hosts, &registry, &runner)?;
                lines.iter().for_each(|l| println!("cached {l}"));
                errors.extend(failed);
            }
            if result.lock != old {
                result.lock.save(&lock_path)?;
            }
            if !errors.is_empty() {
                bail!("{} problem(s):\n{}", errors.len(), errors.join("\n"));
            }
            Ok(())
        }
        Command::Build { job: Some(job), platform, out, .. } => {
            let file: fspec_build::builders::JobFile =
                serde_json::from_slice(&std::fs::read(job)?).context("reading the build job")?;
            let Source::Git { url, .. } = &file.pkg.source else { bail!("{} is not a git package", file.pkg.name) };
            let (local, _) = Facts::probe_local();
            if local != *platform {
                bail!("this machine is {local}, not {platform}");
            }
            let dir = job.parent().context("the job file has no directory")?;
            let cache = fspec_build::cache_root()?;
            let job =
                fspec_build::Job { pkg: &file.pkg, url, rev: &file.rev, platform, spec_dir: dir, out, cache: &cache };
            println!("{}", fspec_build::build(&LiveRunner, &job)?.archive.display());
            Ok(())
        }
        Command::Build { pkg, platform, out, host, .. } => {
            let pkg = pkg.as_ref().context("--pkg is required")?;
            let spec = fspec_eval::evaluate(&spec_path)?;
            let builders = match host {
                Some(h) => spec.host(h)?.builders.clone(),
                None => vec![],
            };
            // One name can come from brew on a Mac and git on Linux; the build wants the git one,
            // from a host of the platform being built when there is one.
            let named: Vec<(&Host, &Pkg)> = spec
                .hosts
                .iter()
                .filter(|h| host.as_ref().is_none_or(|want| h.name == *want))
                .flat_map(|h| h.resources.iter().map(move |r| (h, r)))
                .filter_map(|(h, r)| match r {
                    Resource::Pkg(p) if p.name == *pkg => Some((h, p)),
                    _ => None,
                })
                .collect();
            let git = |p: &&Pkg| matches!(p.source, Source::Git { .. });
            let pkg = named
                .iter()
                .filter(|(h, _)| h.platform == *platform)
                .map(|(_, p)| *p)
                .find(git)
                .or_else(|| named.iter().map(|(_, p)| *p).find(git))
                .with_context(|| match named.first() {
                    None => format!("no host declares a package named `{pkg}`"),
                    Some((_, p)) => format!("{} is {}, only git packages build", p.name, p.source),
                })?;
            let Source::Git { url, .. } = &pkg.source else { unreachable!("picked a git package") };
            let (local, _) = Facts::probe_local();
            if local != *platform {
                bail!("this machine is {local}, build {platform} on a matching runner");
            }
            let lock = Lock::load(&spec_dir.join("fspec.lock"))?;
            let locked = lock
                .pkg(&pkg.name, &pkg.source.to_string())
                .with_context(|| format!("pkg {} is not in fspec.lock, run fspec update", pkg.name))?;
            fspec_core::patches::check(&spec_dir, &pkg.patches, locked.patches.as_deref())?;
            let rev = locked.rev.as_deref().with_context(|| format!("pkg {} has no rev in fspec.lock", pkg.name))?;
            let cache = fspec_build::cache_root()?;
            let job = fspec_build::Job { pkg, url, rev, platform, spec_dir: &spec_dir, out, cache: &cache };
            println!("{}", fspec_build::builders::build_anywhere(&LiveRunner, &builders, &job)?.display());
            Ok(())
        }
        Command::Ci { command: CiCommand::Packages } => {
            let spec = fspec_eval::evaluate(&spec_path)?;
            // The workflow belongs at the repo root; a spec outside any repo keeps it beside itself.
            let top = LiveRunner
                .check(
                    &Cmd::new("git")
                        .arg("-C")
                        .arg(spec_dir.display().to_string())
                        .args(["rev-parse", "--show-toplevel"]),
                )
                .map(|o| PathBuf::from(o.stdout.trim()))
                .unwrap_or_else(|_| spec_dir.clone());
            let rel = spec_path
                .strip_prefix(&top)
                .with_context(|| format!("{} is outside its repo {}", spec_path.display(), top.display()))?;
            // A detached HEAD or a spec outside any repo falls back to main.
            let branch = LiveRunner
                .check(&Cmd::new("git").arg("-C").arg(top.display().to_string()).args([
                    "symbolic-ref",
                    "--short",
                    "HEAD",
                ]))
                .map(|o| o.stdout.trim().to_string())
                .unwrap_or_else(|_| "main".to_string());
            let text = fspec_build::ci::workflow(&fspec_build::ci::jobs(&spec.hosts), &rel.to_string_lossy(), &branch)?;
            let path = top.join(fspec_build::ci::PATH);
            std::fs::create_dir_all(path.parent().context("workflow has a parent")?)?;
            std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
            println!("{}", path.display());
            Ok(())
        }
        Command::Image { command: ImageCommand::Containerfile { host } } => {
            let spec = fspec_eval::evaluate(&spec_path)?;
            let name = host.clone().unwrap_or_else(local_hostname);
            let host = spec.host(&name)?;
            let Some(System::Image(image)) = &host.system else {
                bail!("host {name} has no image(...) system layer");
            };
            print!("{}", fspec_image::containerfile(image)?);
            Ok(())
        }
        Command::Ci { command: CiCommand::Init { registry } } => {
            let spec = fspec_eval::evaluate(&spec_path)?;
            let registry = registry_for(registry.as_deref(), &LiveRunner, &spec_dir)?;
            let hosts: Vec<String> = spec
                .hosts
                .iter()
                .filter(|h| matches!(h.system, Some(System::Image(_))))
                .map(|h| h.name.clone())
                .collect();
            let spec_name = spec_path.file_name().and_then(|n| n.to_str()).context("spec has no file name")?;
            let path = spec_dir.join(fspec_image::WORKFLOW_PATH);
            std::fs::create_dir_all(path.parent().context("workflow path has no parent")?)?;
            std::fs::write(&path, fspec_image::workflow(&registry, spec_name, &hosts)?)
                .with_context(|| format!("writing {}", path.display()))?;
            println!("wrote {}", path.display());
            Ok(())
        }
        Command::Apply { host, all, dirty, local, scope } => {
            let spec = fspec_eval::evaluate(&spec_path)?;
            let this = local_hostname();
            let remote = Remote {
                runner: &LiveRunner,
                spec_dir: spec_dir.clone(),
                spec_file: file_name(&spec_path)?,
                dirty: *dirty,
                loads_cache: None,
            };
            let apply_one = |h: &Host| -> Result<()> {
                match remote_alias(h, &this, &paths, *local) {
                    Some(alias) => remote.apply(h, &alias, scope.map(scope_name)),
                    None => apply_local(&paths, &spec_path, &spec_dir, h, *scope),
                }
            };
            if *all {
                if !is_live(&paths) {
                    bail!("--all applies real machines; it cannot run under --root");
                }
                return fspec_remote::apply_all(&spec.hosts, &this, apply_one);
            }
            apply_one(named(&spec, host.as_deref())?)
        }
        Command::Adopt { path, host } => {
            let spec = fspec_eval::evaluate(&spec_path)?;
            let host = select(&spec, host.as_deref(), &paths)?;
            let env = Env::new(&paths, host, &spec_dir)?;
            adopt(&env, host, path)
        }
        Command::Watch => {
            let stop = std::sync::atomic::AtomicBool::new(false);
            fspec_files::watch::run(&paths, &mut |line| println!("{line}"), &stop)
        }
        Command::Rollback { generation, host, scope } => {
            let spec = fspec_eval::evaluate(&spec_path)?;
            let host = select(&spec, host.as_deref(), &paths)?;
            let mut env = Env::new(&paths, host, &spec_dir)?;
            let scopes: Vec<Scope> = scope.map(|s| vec![s.into()]).unwrap_or(SCOPES.to_vec());
            for scope in scopes {
                let dir = state_dir(&paths, scope);
                let state = State::load(&dir.join("state.json"))?;
                let n = match generation {
                    Some(n) if *n > state.generation => continue,
                    Some(n) => *n,
                    None if state.generation == 0 => continue,
                    None => state.generation - 1,
                };
                if scope == Scope::System && is_live(&paths) && !is_root(env.runner.as_ref())? {
                    sudo_system(&spec_path, &host.name, &paths, &["rollback", &n.to_string()])?;
                    continue;
                }
                let desired = desired_from_generation(&dir, n)?;
                let past = if n == 0 {
                    State::default()
                } else {
                    State::load(&dir.join("generations").join(format!("{n}.json")))?
                };
                // The generation records its packages; the rest of the lock, such as the
                // Tumbleweed snapshot and the image digests, stays as the lock has it.
                env.lock.pkg = fspec_store::lock_from_state(&past).pkg;
                let plan = apply_scope(&env.cx(scope, &state), &backends::all(), &desired)?;
                println!("{plan}");
            }
            Ok(())
        }
        Command::Switch {
            distro,
            adopt_boot,
            reset_var,
            to_image,
            beside,
            inherit,
            pull_from,
            device,
            host,
            release,
            yes,
        } => {
            let opts = Switch {
                distro: distro.as_deref(),
                adopt_boot: *adopt_boot,
                reset_var: *reset_var,
                to_image: *to_image,
                beside: beside.as_deref(),
                inherit: inherit.as_deref(),
                pull_from: pull_from.as_deref(),
                device: device.as_deref(),
            };
            let spec = fspec_eval::evaluate(&spec_path)?;
            let name = host.clone().unwrap_or_else(local_hostname);
            let host = spec.host(&name)?;
            switch(&paths, &spec_path, host, &opts, release.as_deref(), *yes)
        }
        Command::Forget { distro } => forget(&paths, distro),
        Command::Disk { cmd } => disk(&paths, &spec_path, cmd),
        Command::Secret { cmd: SecretCommand::Edit { file, identity } } => {
            secret::edit(&paths, &spec_dir, file, identity)
        }
        Command::Secret { cmd: SecretCommand::Rekey { identity } } => secret::rekey(&paths, &spec_dir, identity),
        Command::Secret { cmd: SecretCommand::Install { scope } } => {
            let scope = Scope::from(*scope);
            let state = State::load(&state_dir(&paths, scope).join("state.json"))?;
            for path in fspec_files::reinstall_secrets(&paths, scope, &state, &LiveRunner)? {
                println!("{}", path.display());
            }
            Ok(())
        }
        Command::Sync { items, peer, from, rollback, dry_run, watch } => {
            let spec = fspec_eval::evaluate(&spec_path)?;
            let this = named(&spec, None)?;
            if is_live(&paths) {
                check_platform(this)?;
            }
            let local = fspec_sync::Local::new(&LiveRunner, this.name.clone(), &paths);
            if *rollback {
                for item in this.sync.iter().filter(|i| items.is_empty() || items.contains(&i.name)) {
                    let n = local.rollback(item)?;
                    println!("{}: {n} restored", item.name);
                }
                return Ok(());
            }
            let opts = fspec_sync::Opts { from: from.clone(), dry_run: *dry_run, peer: peer.clone() };
            let once = || fspec_sync::run(&LiveRunner, &local, &spec.hosts, this, items, &opts);
            let Some(every) = watch else { return once() };
            loop {
                if let Err(e) = once() {
                    eprintln!("fspec: {e:#}");
                }
                std::thread::sleep(std::time::Duration::from_secs(*every));
            }
        }
        Command::SyncAgent { op, item } => {
            let local = fspec_sync::Local::new(&LiveRunner, local_hostname(), &paths);
            let mut out = std::io::stdout().lock();
            fspec_sync::agent(&local, op, item.as_deref(), &mut std::io::stdin().lock(), &mut out)
        }
        Command::Facts { json } => {
            let probe = Probe::local(local_hostname());
            if *json {
                println!("{}", serde_json::to_string(&probe)?);
            } else {
                println!("{}: {}/{}", probe.hostname, probe.platform, probe.distro);
            }
            Ok(())
        }
        Command::Orphans { claim, delete, host } => {
            let spec = fspec_eval::evaluate(&spec_path)?;
            let host = select(&spec, host.as_deref(), &paths)?;
            let env = Env::new(&paths, host, &spec_dir)?;
            orphans(&env, claim.as_deref(), delete.as_deref())
        }
        Command::Gc { older_than, host, scope } => {
            let age = fspec_store::gc::parse_duration(older_than)?;
            let cutoff = std::time::SystemTime::now().checked_sub(age).unwrap_or(std::time::UNIX_EPOCH);
            let host = host.clone().unwrap_or_else(local_hostname);
            let scopes: Vec<Scope> = scope.map(|s| vec![s.into()]).unwrap_or(SCOPES.to_vec());
            for scope in scopes {
                if scope == Scope::System && is_live(&paths) && !is_root(&LiveRunner)? {
                    sudo_system(&spec_path, &host, &paths, &["gc", "--older-than", older_than])?;
                    continue;
                }
                let report = fspec_store::gc::run(&paths, scope, cutoff)?;
                for line in &report.removed {
                    println!("removed {line}");
                }
                println!("{scope:?}: freed {}", fspec_store::gc::human(report.freed));
                if scope == Scope::User && is_live(&paths) {
                    for path in fspec_dev::gc::run(&paths.home, cutoff)? {
                        println!("removed {}", paths.to_spec(&path));
                    }
                }
            }
            Ok(())
        }
        Command::Dev { dir, cmd, args } => dev::run(&paths, dir.as_deref(), cmd.as_ref(), args),
    }
}

struct Switch<'a> {
    distro: Option<&'a str>,
    adopt_boot: bool,
    reset_var: bool,
    to_image: bool,
    beside: Option<&'a str>,
    inherit: Option<&'a str>,
    pull_from: Option<&'a str>,
    device: Option<&'a str>,
}

/// Commands run for real only on a live root; under `--root` they are logged.
fn switch_runner(paths: &Paths) -> Result<Box<dyn Runner>> {
    if !is_live(paths) {
        return Ok(Box::new(LogRunner { log: paths.system("/var/log/fspec-commands.log") }));
    }
    if !is_root(&LiveRunner)? {
        bail!("this changes the boot setup; run it as root (`sudo fspec ...`)");
    }
    Ok(Box::new(LiveRunner))
}

fn run_plan(paths: &Paths, runner: &dyn Runner, plan: &fspec_switch::Plan) -> Result<()> {
    if is_live(paths) {
        return plan.execute_undoing(runner);
    }
    plan.log(runner)?;
    println!("logged {} commands to {}", plan.lines().len(), paths.system("/var/log/fspec-commands.log").display());
    Ok(())
}

fn machine(paths: &Paths, runner: &dyn Runner, host: &Host) -> Result<fspec_switch::Machine> {
    if is_live(paths) {
        return fspec_switch::detect(runner, paths);
    }
    match &host.system {
        Some(System::Distro(d)) => Ok(fspec_switch::assumed(&d.distro)),
        _ => bail!("host {} declares no distro, which a rooted run needs to name its root", host.name),
    }
}

/// A `--root` run cannot read the bootloader, so it assumes the rooted system owns boot.
fn owner(paths: &Paths, runner: &dyn Runner, m: &fspec_switch::Machine) -> Result<Option<String>> {
    if is_live(paths) {
        return fspec_switch::boot_owner(runner, paths, m);
    }
    Ok(Some(m.running.clone()))
}

fn switch(paths: &Paths, spec_path: &Path, host: &Host, opts: &Switch, release: Option<&str>, yes: bool) -> Result<()> {
    let runner = switch_runner(paths)?;
    let spec_dir = spec_path.parent().context("spec has no parent directory")?;
    let live = is_live(paths);
    if let Some(distro) = opts.distro {
        let spec_file = file_name(spec_path)?;
        let exe = std::env::current_exe()?;
        let target = fspec_switch::target(host, distro, release, spec_dir, &spec_file, &exe)?;
        let m = machine(paths, runner.as_ref(), host)?;
        let plan = fspec_switch::switch_distro(paths, &m, owner(paths, runner.as_ref(), &m)?.as_deref(), &target)?;
        run_plan(paths, runner.as_ref(), &plan)?;
        if live {
            println!(
                "{distro} is installed beside {}: reboot and pick \"{}\" from the boot menu. Run `fspec switch \
                 --adopt-boot` there to hand boot over before you `fspec forget` this root",
                m.running,
                fspec_switch::title(distro)
            );
        }
    } else if opts.adopt_boot {
        if live && fspec_switch::is_bootc(runner.as_ref())? {
            bail!(
                "this is a bootc deployment, where bootupd owns the bootloader; switch --adopt-boot does not handle \
                 bootc roots yet"
            );
        }
        let m = machine(paths, runner.as_ref(), host)?;
        run_plan(paths, runner.as_ref(), &fspec_switch::adopt_boot(paths, &m)?)?;
        if live {
            println!("this root ({}) owns boot now", m.running);
        }
    } else if opts.reset_var {
        let Some(System::Image(_)) = &host.system else { bail!("host {} has no image(...) system layer", host.name) };
        if live {
            if !fspec_switch::is_bootc(runner.as_ref())? {
                bail!("this machine is not a bootc deployment, so there is no image default to reset to");
            }
            if !fspec_switch::has_staged(runner.as_ref())? {
                bail!("no image is staged; change image(base = ...), run `fspec apply`, then switch --reset-var");
            }
        }
        let reset = fspec_switch::reset_var(paths, &host.persist)?;
        run_plan(paths, runner.as_ref(), &reset.plan)?;
        if live {
            println!("/etc and /var reset at the next boot; remove /var/lib/fspec/reset-var/pending to cancel");
        }
    } else if let (Some(name), false) = (opts.beside, opts.to_image) {
        let tumbleweed = matches!(&host.system, Some(System::Distro(d)) if d.distro == "opensuse-tumbleweed");
        if !tumbleweed {
            bail!("--beside installs the host's image with --to-image, or a host that declares tumbleweed()");
        }
        let lock = Lock::load(&spec_dir.join("fspec.lock"))?;
        let Some(snapshot) = lock.zypper.get(&host.name).map(|z| z.snapshot.as_str()) else {
            bail!("fspec.lock pins no Tumbleweed snapshot for host {}; run `fspec update`", host.name)
        };
        let m = match opts.device {
            Some(device) if live => {
                fspec_switch::detect_device(runner.as_ref(), paths, device, opts.inherit.unwrap_or_default())?
            }
            None if live => fspec_switch::detect(runner.as_ref(), paths)?,
            _ => fspec_switch::assumed(&host.name),
        };
        let inherit = match opts.inherit {
            Some(i) => i.to_string(),
            None => m.root.trim_start_matches('/').to_string(),
        };
        let t = fspec_switch::Tumbleweed {
            name,
            snapshot,
            inherit: &inherit,
            users: host.users.iter().map(|u| u.name.clone()).collect(),
            kernel_args: match &host.system {
                Some(System::Distro(d)) => &d.kernel_args,
                _ => &[],
            },
        };
        run_plan(paths, runner.as_ref(), &fspec_switch::tumbleweed(paths, &m, &t)?)?;
        if live {
            println!(
                "openSUSE Tumbleweed {snapshot} is installed in @{name} beside {inherit}. Boot it once with \
                 `bootctl set-oneshot {name}-<kernel>.conf` (`bootctl list` shows the entry); it is blessed only once \
                 sshd, and tailscale when it has state, are up, and otherwise the next boot is the default again"
            );
        }
    } else if opts.to_image {
        let Some(System::Image(_)) = &host.system else { bail!("host {} has no image(...) system layer", host.name) };
        let lock = Lock::load(&spec_dir.join("fspec.lock"))?;
        let Some(locked) = lock.system.get(&host.name) else {
            bail!("fspec.lock has no image for host {}; run `fspec update --images` first", host.name)
        };
        if let Some(name) = opts.beside {
            let m =
                if live { fspec_switch::detect(runner.as_ref(), paths)? } else { fspec_switch::assumed(&host.name) };
            let b = fspec_switch::Beside {
                image: &locked.image,
                distro: name,
                inherit: opts.inherit,
                users: host.users.iter().map(|u| u.name.clone()).collect(),
                pull_from: opts.pull_from,
            };
            run_plan(paths, runner.as_ref(), &fspec_switch::beside(paths, &m, &b)?)?;
            if live {
                println!(
                    "{} is installed in @{name} beside {}. Boot it once with `bootctl set-oneshot {name}-ostree-1.conf` \
                     (`bootctl list` shows the entry); it is blessed only once sshd and tailscale are up, and \
                     otherwise the next boot is the default again",
                    locked.image, m.running
                );
            }
            return Ok(());
        }
        if live {
            if fspec_switch::is_bootc(runner.as_ref())? {
                bail!("this machine already runs bootc; change image(base = ...) and apply instead");
            }
            let m = fspec_switch::detect(runner.as_ref(), paths)?;
            match &m.layout {
                fspec_switch::Layout::Default { .. } => {}
                fspec_switch::Layout::Zfs { .. } => bail!("bootc does not install to a ZFS root"),
                fspec_switch::Layout::Bios { .. } => bail!(
                    "bootc's bootupd would install its own GRUB over the shared one of the BIOS layout; --to-image \
                     needs the default EFI layout"
                ),
                fspec_switch::Layout::Unsupported { reasons } => bail!(
                    "this disk is not on a supported layout: {}; run `fspec disk plan` for the steps",
                    reasons.join("; ")
                ),
            }
            if !yes {
                bail!("bootc install to-existing-root reinitialises /boot and replaces this system; rerun with --yes");
            }
        }
        run_plan(paths, runner.as_ref(), &fspec_switch::to_image(paths, &locked.image))?;
    } else {
        bail!("pick one of --distro <distro>, --adopt-boot, --reset-var or --to-image");
    }
    Ok(())
}

/// The disk layout, which root owns boot, and whether the declared distro fits, for `plan` on a
/// classic distro. Reading them mounts filesystems, so only root sees them.
fn boot_line(paths: &Paths, host: &Host) -> Option<String> {
    let Some(System::Distro(declared)) = &host.system else { return None };
    if !is_live(paths) || !is_root(&LiveRunner).ok()? {
        return None;
    }
    let m = fspec_switch::detect(&LiveRunner, paths).ok()?;
    let mut lines = vec![match &m.layout {
        fspec_switch::Layout::Unsupported { reasons } => {
            format!("disk: not on a supported layout ({}); see `fspec disk plan`", reasons.join("; "))
        }
        l => format!("disk: the {} layout", l.name()),
    }];
    if let Some(o) = fspec_switch::boot_owner(&LiveRunner, paths, &m).ok()? {
        let here = if o == m.running { ", this root" } else { "" };
        lines.push(format!("boot is owned by {o}{here}"));
    }
    let d = declared.distro.as_str();
    if d != m.running {
        match fspec_switch::recipe(d).ok().map(|r| fspec_switch::misfit(r, &m.layout, &m.firmware)) {
            Some(None) => lines.push(format!("switch --distro {d}: fits this disk")),
            Some(Some(why)) => lines.push(format!("switch --distro {d}: refused, {why}")),
            None => {}
        }
    }
    lines.extend(fspec_switch::caveats(d, &m.layout));
    Some(lines.join("\n"))
}

fn forget(paths: &Paths, distro: &str) -> Result<()> {
    let runner = switch_runner(paths)?;
    if !is_live(paths) {
        bail!("forget reads the disk it deletes from, so it only runs on a live system");
    }
    let m = fspec_switch::detect(runner.as_ref(), paths)?;
    let plan = fspec_switch::forget(paths, &m, owner(paths, runner.as_ref(), &m)?.as_deref(), distro)?;
    run_plan(paths, runner.as_ref(), &plan)?;
    println!("{distro} and its boot entries are deleted");
    Ok(())
}

/// Ask for `device` to be typed back before a step that destroys data on it.
fn confirm(device: &str, what: &str) -> Result<()> {
    use std::io::Write;
    print!("{what}\nType {device} to continue: ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    if line.trim() != device {
        bail!("you typed `{}`, not {device}; nothing was changed", line.trim());
    }
    Ok(())
}

/// `path` was modified in the last 24 hours.
fn require_backup(device: &str, backup: Option<&Path>, no_backup: bool) -> Result<()> {
    match backup {
        Some(b) => recent_backup(b),
        None if no_backup => {
            eprintln!("converting {device} without a backup (--no-backup)");
            Ok(())
        }
        None => {
            bail!("converting {device} needs a backup taken in the last 24 hours, named with --backup, or --no-backup")
        }
    }
}

fn recent_backup(path: &Path) -> Result<()> {
    let age = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .with_context(|| format!("reading the backup {}", path.display()))?
        .elapsed()
        .unwrap_or_default();
    if age > std::time::Duration::from_secs(24 * 3600) {
        bail!("the backup {} is {} hours old; take a new one first", path.display(), age.as_secs() / 3600);
    }
    Ok(())
}

/// A read-only snapshot under @snapshots created in the last 24 hours. A snapshot's root inode
/// keeps the birth time of the subvolume it was taken from, so the age comes from btrfs instead.
fn recent_snapshot(runner: &dyn Runner, paths: &Paths, m: &fspec_switch::Machine) -> Result<bool> {
    if m.fstype != "btrfs" {
        return Ok(false);
    }
    let top = paths.system("/run/fspec/disk/look").display().to_string();
    runner.check(&Cmd::new("mkdir").arg("-p").arg(&top))?;
    runner.check(&Cmd::new("mount").args(["-o", "ro,subvolid=5"]).arg(format!("UUID={}", m.fs_uuid)).arg(&top))?;
    let check = r#"now=$(date +%s); for s in "$1"/@snapshots/* "$1"/@snapshots/*/*; do [ -d "$s" ] || continue; btrfs property get -ts "$s" ro 2>/dev/null | grep -q true || continue; t=$(btrfs subvolume show "$s" | sed -n 's/^[[:space:]]*Creation time:[[:space:]]*//p'); [ "$(date -d "$t" +%s)" -ge $((now - 86400)) ] && exit 0; done; exit 1"#;
    let found = runner.run(&Cmd::new("sh").args(["-c", check, "sh", &top]));
    runner.run(&Cmd::new("umount").arg(&top))?;
    Ok(found?.status == 0)
}

fn disk(paths: &Paths, spec_path: &Path, cmd: &DiskCommand) -> Result<()> {
    if !is_live(paths) {
        bail!("fspec disk works on real disks, so it does not run under --root");
    }
    let runner = switch_runner(paths)?;
    let runner = runner.as_ref();
    match cmd {
        DiskCommand::Unmask { esp } => {
            println!("{}", fspec_switch::unmask(paths, esp)?);
            Ok(())
        }
        DiskCommand::Plan { device: Some(device), nixos_system }
        | DiskCommand::Migrate { device: Some(device), nixos_system, .. } => {
            // A NixOS migration journals on the ESP, which outlives a reboot of the live system.
            let esp = fspec_switch::mount_esp(runner, paths, device)?;
            let state = esp.as_ref().map(|(_, at)| Path::new(at).join("fspec/disk-journal"));
            if let Some(state) = state.as_ref().filter(|s| s.join("journal.json").exists()) {
                if let DiskCommand::Plan { .. } = cmd {
                    println!(
                        "an interrupted migration left {}; `fspec disk migrate {device}` rolls it back",
                        state.display()
                    );
                    return Ok(());
                }
                fspec_core::journal::Journal::recover(state, runner)
                    .context("rolling back the interrupted migration")?;
                fspec_core::journal::remove_any(state)?;
                println!("rolled back the interrupted migration; {device} is as it was before it");
                return Ok(());
            }
            let fstype = runner.check(&Cmd::new("lsblk").args(["-no", "FSTYPE,MOUNTPOINTS", device]))?.stdout;
            let mut f = fstype.split_whitespace();
            if f.next() != Some("ext4") {
                bail!("{device} is not an ext4 filesystem; `disk migrate <device>` only converts an ext4 root");
            }
            if f.next().is_some() {
                bail!("{device} is mounted; it converts only from a live system with it unmounted");
            }
            let uuid = runner.check(&Cmd::new("lsblk").args(["-no", "UUID", device]))?.stdout.trim().to_string();
            if let Some(root) = fspec_switch::nixos_root(runner, paths, device, &uuid, nixos_system.as_deref())? {
                let (Some((esp, at)), Some(state)) = (&esp, &state) else {
                    bail!("the disk holding {device} has no ESP, which NixOS's systemd-boot entries need")
                };
                let named = runner.check(&Cmd::new("readlink").args(["-f", &root.esp]))?.stdout.trim().to_string();
                if &named != esp {
                    bail!("the system mounts {} ({named}) as its ESP, but the disk's ESP is {esp}", root.esp);
                }
                return migrate_nixos(paths, runner, cmd, &root, at, state);
            }
            if nixos_system.is_some() {
                bail!("{device} does not hold NixOS; --nixos-system only applies to a NixOS root");
            }
            let m = fspec_switch::detect(runner, paths)?;
            let disk = runner.check(&Cmd::new("lsblk").args(["-ndpo", "PKNAME", device]))?.stdout.trim().to_string();
            let plan = fspec_switch::convert(paths, device, &disk, &m.firmware);
            match cmd {
                DiskCommand::Migrate { backup, no_backup, .. } => {
                    require_backup(device, backup.as_deref(), *no_backup)?;
                    confirm(device, &format!("This converts the ext4 filesystem on {device} to btrfs."))?;
                    plan.execute(runner)?;
                    println!("{device} is btrfs now; boot it and run `fspec disk migrate` for the rest");
                }
                _ => {
                    println!("1. convert the ext4 root {device} to btrfs in place, keeping its UUID (destructive)");
                    println!("then boot it and run `fspec disk plan` again");
                }
            }
            Ok(())
        }
        DiskCommand::Migrate { stage: Some(iso), nixos_system: Some(system), overlay, backup, no_backup, .. } => {
            stage_migration(paths, runner, iso, overlay.as_deref(), system, backup.as_deref(), *no_backup)
        }
        DiskCommand::Migrate { device: None, nixos_system: Some(_), .. } => {
            bail!("--nixos-system names the system for a NixOS root on DEVICE, or for the running one with --stage")
        }
        DiskCommand::Plan { device: None, .. } | DiskCommand::Migrate { device: None, .. } => {
            let m = fspec_switch::detect(runner, paths)?;
            let left = fspec_switch::leftovers(runner, paths, &m)?;
            let swap = match cmd {
                DiskCommand::Migrate { swap, .. } => *swap,
                _ => 4,
            };
            let steps = fspec_switch::migrate(paths, &m, &left, &fspec_switch::new_ids()?, swap)?;
            if steps.is_empty() {
                println!("this disk is on the {} layout; nothing to migrate", m.layout.name());
                return Ok(());
            }
            let DiskCommand::Migrate { backup, .. } = cmd else {
                for (i, s) in steps.iter().enumerate() {
                    let mark = if s.destructive { " (destructive)" } else { "" };
                    println!("{}. {}{mark}", i + 1, s.what);
                }
                return Ok(());
            };
            for (i, step) in steps.iter().enumerate() {
                println!("step {} of {}: {}", i + 1, steps.len(), step.what);
                if step.destructive {
                    match backup {
                        Some(b) => recent_backup(b)?,
                        None if recent_snapshot(runner, paths, &m)? => {}
                        None => bail!(
                            "this step changes {} in place; take a read-only snapshot under @snapshots or a backup \
                             first (name it with --backup)",
                            m.disk
                        ),
                    }
                    confirm(&m.disk, &format!("This step changes the partitions or files on {}.", m.disk))?;
                }
                step.plan.execute(runner)?;
            }
            println!("done; reboot, then run `fspec disk plan` to see what the move left behind");
            Ok(())
        }
        DiskCommand::Init { device, layout, luks, distro, host, release, backup, swap } => {
            if !fspec_switch::blank_disk(runner, device)? {
                let Some(backup) = backup else {
                    bail!("{device} holds data; name a backup taken in the last 24 hours with --backup")
                };
                recent_backup(backup)?;
            }
            let kind = match layout {
                LayoutArg::Default => fspec_switch::Kind::Default,
                LayoutArg::Zfs => fspec_switch::Kind::Zfs,
                LayoutArg::Bios => fspec_switch::Kind::Bios,
            };
            let spec_dir = spec_path.parent().context("spec has no parent directory")?;
            let spec_file = file_name(spec_path)?;
            let exe = std::env::current_exe()?;
            let spec;
            let target = match (distro, host) {
                (Some(d), Some(h)) => {
                    spec = fspec_eval::evaluate(spec_path)?;
                    Some(fspec_switch::target(spec.host(h)?, d, release.as_deref(), spec_dir, &spec_file, &exe)?)
                }
                _ => None,
            };
            let ids = fspec_switch::new_ids()?;
            let efi_vars = std::path::Path::new("/sys/firmware/efi").is_dir();
            let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
            let init = fspec_switch::Init {
                device,
                kind,
                luks: *luks,
                ids: &ids,
                swap_gib: *swap,
                efi_vars,
                target: target.as_ref(),
                cmdline: &cmdline,
                in_root: &[],
                passphrase: None,
                windows: None,
            };
            let plan = fspec_switch::init(paths, &init)?;
            confirm(device, &format!("This erases everything on {device}."))?;
            plan.execute(runner)?;
            println!("{device} has the {} layout", kind_name(kind));
            Ok(())
        }
    }
}

/// `disk migrate --stage`: on the running NixOS root on ext4, stage the live ISO and a one-shot
/// boot entry that migrates this root unattended on the next boot.
fn stage_migration(
    paths: &Paths,
    runner: &dyn Runner,
    iso: &Path,
    overlay: Option<&Path>,
    system: &str,
    backup: Option<&Path>,
    no_backup: bool,
) -> Result<()> {
    let found = |c: Cmd| -> Result<String> { Ok(runner.check(&c)?.stdout.trim().to_string()) };
    let root = found(Cmd::new("findmnt").args(["-nvo", "SOURCE,FSTYPE", "/"]))?;
    let (device, fstype) = root.split_once(' ').map(|(d, f)| (d, f.trim())).unwrap_or((&root, ""));
    if fstype != "ext4" {
        bail!("/ is {fstype}, not ext4; --stage migrates a NixOS root on ext4");
    }
    let uuid = found(Cmd::new("lsblk").args(["-no", "UUID", device]))?;
    let Some(nixos) = fspec_switch::nixos_at(&paths.system("/"), device, &uuid, Some(system))? else {
        bail!("this root is not NixOS; --stage only migrates a NixOS root")
    };
    let esp = found(Cmd::new("bootctl").arg("--print-esp-path"))?;
    if esp != nixos.esp_at {
        bail!("{system} mounts the ESP at {}, but systemd-boot's ESP is {esp}", nixos.esp_at);
    }
    let esp_uuid = found(Cmd::new("findmnt").args(["-no", "UUID", &esp]))?;
    let exe = std::env::current_exe()?;
    if !static_elf(&exe)? {
        bail!(
            "{} is linked dynamically; the live system runs the fspec that staged it, which needs a static build \
             such as the one fspec installs at /usr/local/bin/fspec",
            exe.display()
        );
    }
    let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname")?.trim().to_string();
    let cmdline = std::fs::read_to_string("/proc/cmdline")?;
    let keep: Vec<String> =
        cmdline.split_whitespace().filter(|o| o.starts_with("console=")).map(String::from).collect();
    require_backup(device, backup, no_backup)?;
    let (iso, overlay, exe) =
        (iso.display().to_string(), overlay.map(|o| o.display().to_string()), exe.display().to_string());
    let stage = fspec_switch::Stage {
        root: &nixos,
        uuid: &uuid,
        iso: &iso,
        overlay: overlay.as_deref(),
        fspec: &exe,
        esp: &esp,
        esp_uuid: &esp_uuid,
        hostname: &hostname,
        keep: &keep,
    };
    let plan = fspec_switch::stage_live(paths, &stage);
    confirm(
        device,
        &format!(
            "On the next boot, a live system converts {device} to btrfs and moves NixOS into @nixos, with nobody \
             asked again."
        ),
    )?;
    plan.execute(runner)?;
    println!(
        "staged; the next boot runs the live system once, migrates {device} and reboots into NixOS generation {} \
         from @nixos. Its log lands in {esp}/fspec/migrate.log",
        nixos.generation + 1
    );
    Ok(())
}

/// An ELF executable without an interpreter, which runs on any Linux root.
fn static_elf(path: &Path) -> Result<bool> {
    let elf = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let at = |o: usize, n: usize| elf.get(o..o + n).context("not a 64-bit ELF file");
    if at(0, 5)? != b"\x7fELF\x02" {
        bail!("{} is not a 64-bit ELF executable", path.display());
    }
    let phoff = u64::from_le_bytes(at(0x20, 8)?.try_into()?) as usize;
    let size = u16::from_le_bytes(at(0x36, 2)?.try_into()?) as usize;
    let count = u16::from_le_bytes(at(0x38, 2)?.try_into()?) as usize;
    const PT_INTERP: u32 = 3;
    for i in 0..count {
        if u32::from_le_bytes(at(phoff + i * size, 4)?.try_into()?) == PT_INTERP {
            return Ok(false);
        }
    }
    Ok(true)
}

/// `disk plan` or `disk migrate` for the NixOS root on a live system's unmounted ext4 partition.
fn migrate_nixos(
    paths: &Paths,
    runner: &dyn Runner,
    cmd: &DiskCommand,
    root: &fspec_switch::NixosRoot,
    esp: &str,
    state: &Path,
) -> Result<()> {
    let device = root.device.as_str();
    let steps = fspec_switch::nixos_migrate(paths, root, esp, &state.display().to_string());
    let DiskCommand::Migrate { backup, no_backup, .. } = cmd else {
        for (i, s) in steps.iter().enumerate() {
            let mark = if s.destructive { " (destructive)" } else { "" };
            println!("{}. {}{mark}", i + 1, s.what);
        }
        println!(
            "then boot generation {} and run `fspec disk plan`, which lists deleting the old root's files and \
             ext2_saved from the top level, the copy of /home in @nixos and its old /swapfile (destructive)",
            root.generation + 1
        );
        return Ok(());
    };
    require_backup(device, backup.as_deref(), *no_backup)?;
    confirm(device, &format!("This converts the ext4 filesystem on {device} to btrfs and moves NixOS into @nixos."))?;
    let mut journal = fspec_core::journal::Journal::begin(state, 0)?;
    for (i, step) in steps.iter().enumerate() {
        println!("step {} of {}: {}", i + 1, steps.len(), step.what);
        if let Err(e) = step.plan.execute_journaled(runner, &mut journal) {
            return match journal.rollback(runner) {
                Ok(()) => Err(e.context(format!("rolled back: {device} is ext4 again, with NixOS as it was"))),
                Err(r) => Err(e.context(format!("the rollback failed too: {r:#}"))),
            };
        }
    }
    journal.finish()?;
    fspec_core::journal::remove_any(state)?;
    runner.check(&fspec_switch::compress(paths, device))?;
    println!("started fspec-compress.service, which recompresses @home and @nixos with zstd; a reboot stops it");
    println!(
        "done; reboot into NixOS generation {} from @nixos. Until ext2_saved is deleted, `btrfs-convert -r {device}` \
         from a live system goes back to ext4",
        root.generation + 1
    );
    Ok(())
}

fn kind_name(k: fspec_switch::Kind) -> &'static str {
    match k {
        fspec_switch::Kind::Default => "default",
        fspec_switch::Kind::Zfs => "zfs",
        fspec_switch::Kind::Bios => "bios",
    }
}

fn scope_name(s: ScopeArg) -> &'static str {
    match s {
        ScopeArg::System => "system",
        ScopeArg::User => "user",
    }
}

fn file_name(spec_path: &Path) -> Result<String> {
    Ok(spec_path.file_name().context("spec has no file name")?.to_string_lossy().into_owned())
}

/// The ssh alias when `host` is another machine. A rooted run is always a local dry run.
fn remote_alias(host: &Host, this: &str, paths: &Paths, local: bool) -> Option<String> {
    match fspec_remote::target(host, this) {
        Target::Remote { alias } if is_live(paths) && !local => Some(alias),
        _ => None,
    }
}

fn plan_local(paths: &Paths, spec_path: &Path, host: &Host, scope: Option<ScopeArg>) -> Result<Plan> {
    if is_live(paths) {
        check_declared(host)?;
    }
    let spec_dir = spec_path.parent().context("spec has no parent directory")?;
    let env = Env::new(paths, host, spec_dir)?;
    let scopes: Vec<Scope> = scope.map(|s| vec![s.into()]).unwrap_or(SCOPES.to_vec());
    let mut plans = Vec::new();
    for scope in scopes {
        // The system scope reads root-only files (sudoers drop-ins, the ESP), as apply writes them.
        if scope == Scope::System && is_live(paths) && !is_root(env.runner.as_ref())? {
            let out = sudo_system(spec_path, &host.name, paths, &["plan", "--json", "--local"])?;
            plans.push(serde_json::from_slice(&out).context("reading the system scope's plan from sudo")?);
            continue;
        }
        let state = State::load(&state_dir(paths, scope).join("state.json"))?;
        plans.push(plan_scope(
            &env.cx(scope, &state),
            &backends::all(),
            &engine::unless_unlocked(host.desired(), &env.lock),
            &state,
        )?);
    }
    Ok(merge(plans))
}

fn plan_all(spec: &fspec_eval::Spec, paths: &Paths, spec_path: &Path, dirty: bool) -> Result<()> {
    let spec_dir = spec_path.parent().context("spec has no parent directory")?;
    let this = local_hostname();
    let remote = Remote {
        runner: &LiveRunner,
        spec_dir: spec_dir.to_path_buf(),
        spec_file: file_name(spec_path)?,
        dirty,
        loads_cache: None,
    };
    let mut failed = 0;
    for host in &spec.hosts {
        let outcome = match remote_alias(host, &this, paths, false) {
            Some(alias) => remote.plan(host, &alias).map(|p| match p {
                Planned::Plan(plan) => plan.to_string(),
                Planned::Unreachable => format!("host {}\n  unreachable, skipped", host.name),
            }),
            None => plan_local(paths, spec_path, host, None).map(|p| p.to_string()),
        };
        match outcome {
            Ok(text) => println!("{text}"),
            Err(e) => {
                failed += 1;
                println!("host {}\n  error: {e:#}", host.name);
            }
        }
    }
    if failed > 0 {
        bail!("{failed} host(s) could not be planned");
    }
    Ok(())
}

fn orphans(env: &Env, claim: Option<&[String]>, delete: Option<&str>) -> Result<()> {
    use fspec_core::journal::Journal;
    use fspec_core::{attribute, trash};
    let system = State::load(&state_dir(env.paths, Scope::System).join("state.json"))?;
    let dir = state_dir(env.paths, Scope::User);
    let user_path = dir.join("state.json");
    let mut user = State::load(&user_path)?;
    let orphans = attribute::orphans(env.paths, &system, &user);
    match (claim, delete) {
        (Some([path, pkg]), _) => {
            let key = fspec_core::Key { kind: "pkg".into(), id: pkg.clone() };
            if !system.entries.contains_key(&key) && !user.entries.contains_key(&key) {
                bail!("{key} is not installed");
            }
            let spec = attribute::claim(env.paths, &mut user, path, pkg)?;
            user.save(&user_path)?;
            println!("{spec} now belongs to {pkg}");
        }
        (_, Some(path)) => {
            let spec = attribute::orphan_spec(env.paths, path)?;
            if !orphans.contains(&spec) {
                bail!("{spec} is claimed by a package or covered by a managed file, so it is not an orphan");
            }
            let cx = env.cx(Scope::User, &user);
            Journal::recover(&dir, cx.runner)?;
            let mut journal = Journal::begin(&dir, cx.generation)?;
            match trash::trash_paths(&cx, &mut journal, None, &[env.paths.resolve(&spec)?]) {
                Ok(()) => journal.finish()?,
                Err(e) => {
                    journal.rollback(cx.runner)?;
                    return Err(e);
                }
            }
            println!("moved {spec} to the trash");
        }
        _ => orphans.iter().for_each(|o| println!("{o}")),
    }
    Ok(())
}

fn apply_local(paths: &Paths, spec_path: &Path, spec_dir: &Path, host: &Host, scope: Option<ScopeArg>) -> Result<()> {
    if is_live(paths) {
        check_declared(host)?;
    }
    let env = Env::new(paths, host, spec_dir)?;
    let scopes: Vec<Scope> = scope.map(|s| vec![s.into()]).unwrap_or(SCOPES.to_vec());
    for scope in scopes {
        if scope == Scope::System && is_live(paths) && !is_root(env.runner.as_ref())? {
            sudo_system(spec_path, &host.name, paths, &["apply", "--local"])?;
            continue;
        }
        let state = State::load(&state_dir(paths, scope).join("state.json"))?;
        let plan =
            apply_scope(&env.cx(scope, &state), &backends::all(), &engine::unless_unlocked(host.desired(), &env.lock))?;
        println!("{plan}");
    }
    Ok(())
}

fn registry_for(flag: Option<&str>, runner: &dyn Runner, spec_dir: &Path) -> Result<String> {
    match flag {
        Some(r) => Ok(r.to_string()),
        None => fspec_image::default_registry(runner, spec_dir),
    }
}

fn adopt(env: &Env, host: &Host, path: &str) -> Result<()> {
    use fspec_files::adopt::{Outcome, adopt};
    let wanted = std::path::absolute(path)?;
    let (file, scope) = host
        .desired()
        .into_iter()
        .find_map(|d| match d.resource {
            Resource::File(f) => {
                let hit = f.path == path || env.paths.resolve(&f.path).is_ok_and(|p| p == wanted);
                hit.then(|| {
                    let scope = Resource::File(f.clone()).scope();
                    (f, scope)
                })
            }
            _ => None,
        })
        .with_context(|| format!("host {} declares no file at {path}", host.name))?;
    let state_path = state_dir(env.paths, scope).join("state.json");
    let mut state = State::load(&state_path)?;
    let key = Resource::File(file.clone()).key();
    let record = state.entries.get(&key).map(|e| e.record.clone()).unwrap_or_default();
    match adopt(&env.cx(scope, &state), &file, &record)? {
        Outcome::InSync => println!("{} is what fspec wrote, nothing to adopt", file.path),
        Outcome::Copied { to, record } => {
            if let Some(entry) = state.entries.get_mut(&key) {
                entry.record = record;
                state.save(&state_path)?;
            }
            println!("copied {} into {}", file.path, to.display());
        }
        Outcome::Patch { notes } => {
            println!("{} owns keys, not the file; these changed on disk:", file.path);
            notes.iter().for_each(|n| println!("  {n}"));
        }
        Outcome::Generated { diff } => {
            print!("{diff}");
            bail!(
                "{} is generated from the spec, so there is no repo file to update; edit the spec to match",
                file.path
            );
        }
    }
    Ok(())
}

/// What every scope's `Cx` borrows.
struct Env<'a> {
    paths: &'a Paths,
    facts: Facts,
    spec_dir: &'a Path,
    lock: Lock,
    runner: Box<dyn Runner>,
    fetch: LiveFetch,
    protect: Vec<String>,
    builders: Vec<fspec_core::resource::Builder>,
}

impl<'a> Env<'a> {
    fn new(paths: &'a Paths, host: &Host, spec_dir: &'a Path) -> Result<Env<'a>> {
        Ok(Env {
            paths,
            facts: Facts::declared(&host.name, &host.platform, host.system.as_ref())?,
            spec_dir,
            lock: Lock::load(&spec_dir.join("fspec.lock"))?,
            runner: if is_live(paths) {
                Box::new(LiveRunner)
            } else {
                Box::new(LogRunner { log: paths.system("/var/log/fspec-commands.log") })
            },
            fetch: LiveFetch::new(),
            protect: host.protect.clone(),
            builders: host.builders.clone(),
        })
    }

    fn cx(&self, scope: Scope, state: &State) -> Cx<'_> {
        Cx {
            paths: self.paths,
            facts: &self.facts,
            spec_dir: self.spec_dir,
            lock: &self.lock,
            runner: self.runner.as_ref(),
            fetch: &self.fetch,
            generation: state.generation + 1,
            scope,
            protect: &self.protect,
            builders: &self.builders,
            build_cache: None,
        }
    }
}

fn merge(plans: Vec<Plan>) -> Plan {
    let mut all = Plan { host: plans[0].host.clone(), changes: vec![], unsupported: vec![] };
    for p in plans {
        all.changes.extend(p.changes);
        all.unsupported.extend(p.unsupported);
    }
    all.changes.sort_by(|a, b| a.key.cmp(&b.key));
    all
}

/// The host named on the command line, or this machine's.
fn named<'s>(spec: &'s fspec_eval::Spec, host: Option<&str>) -> Result<&'s Host> {
    spec.host(&host.map(str::to_owned).unwrap_or_else(local_hostname))
}

/// [`named`], for commands that only ever act on this machine.
fn select<'s>(spec: &'s fspec_eval::Spec, host: Option<&str>, paths: &Paths) -> Result<&'s Host> {
    let host = named(spec, host)?;
    if is_live(paths) {
        check_declared(host)?;
    }
    Ok(host)
}

fn paths(cli: &Cli) -> Result<Paths> {
    let live = Paths::live()?;
    let home = match (&cli.home, is_live_root(&cli.root)) {
        (Some(h), _) => h.clone(),
        (None, true) => live.home,
        (None, false) => {
            let user = live.home.file_name().map(PathBuf::from).unwrap_or_else(|| "user".into());
            cli.root.join("home").join(user)
        }
    };
    Ok(Paths::new(cli.root.clone(), home))
}

fn is_live_root(root: &Path) -> bool {
    root == Path::new("/")
}

fn is_live(paths: &Paths) -> bool {
    is_live_root(&paths.root)
}

fn is_root(runner: &dyn Runner) -> Result<bool> {
    Ok(runner.check(&Cmd::new("id").arg("-u"))?.stdout.trim() == "0")
}

/// The system scope needs root and the user scope must not run as root, so a non-root apply,
/// plan or rollback hands the system scope to `sudo fspec <command> --scope system`. What it
/// prints is returned for `plan --json` and passed through otherwise.
fn sudo_system(spec: &Path, host: &str, paths: &Paths, command: &[&str]) -> Result<Vec<u8>> {
    let exe = std::env::current_exe()?;
    let mut sudo = std::process::Command::new("sudo");
    sudo.arg(exe)
        .arg("--spec")
        .arg(spec)
        .arg("--home")
        .arg(&paths.home)
        .args(command)
        .args(["--scope", "system", "--host", host]);
    let capture = command.contains(&"--json");
    if capture {
        sudo.stdout(std::process::Stdio::piped());
    }
    let out = sudo.spawn().and_then(|c| c.wait_with_output()).context("running sudo")?;
    if !out.status.success() {
        bail!("system scope {} failed", command[0]);
    }
    Ok(out.stdout)
}

/// A host's declared platform must match the machine before fspec acts on it.
fn check_platform(host: &Host) -> Result<()> {
    let facts = Facts::declared(&host.name, &host.platform, host.system.as_ref())?;
    let (platform, _) = Facts::probe_local();
    if facts.platform() != platform {
        bail!("host {} is declared as {} but this machine is {platform}", host.name, facts.platform());
    }
    Ok(())
}

/// The platform and the distro too, for commands that drive the distro's packages and boot.
fn check_declared(host: &Host) -> Result<()> {
    check_platform(host)?;
    let facts = Facts::declared(&host.name, &host.platform, host.system.as_ref())?;
    let (_, distro) = Facts::probe_local();
    if facts.distro != distro {
        bail!("host {} is declared as {} but this machine runs {distro}", host.name, facts.distro);
    }
    Ok(())
}
