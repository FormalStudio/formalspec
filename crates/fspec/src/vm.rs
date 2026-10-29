//! `fspec vm`: the CLI end of [`fspec_try::vm`]. It reads the host's `vm(...)` from the spec,
//! resolves the cloud image and the first boot's setup here, and runs the script on the machine
//! the VM lives on.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use fspec_core::Host;
use fspec_core::facts::local_hostname;
use fspec_core::fetch::{Fetch, LiveFetch};
use fspec_core::resource::{System, Vm};
use fspec_core::runner::{Cmd, LiveRunner, Runner};
use fspec_remote::cmd::batch;
use fspec_try::Iso;
use fspec_try::vm::{Guest, Record};

#[derive(Subcommand)]
pub enum VmCommand {
    /// Boot the host's VM where its vm(on = ...) says, creating its disk on the first start, and
    /// wait until its sshd answers.
    Start { host: String },
    /// Power the host's VM off; its disk stays.
    Stop { host: String },
    /// Record the VM's display as numbered PNG frames on the machine it runs on, in the
    /// background; `fspec vm frames` fetches them. With --boot the VM has to be off: it is
    /// powered on paused and runs once the first frame is taken.
    Record {
        host: String,
        /// Power the VM on and record from the firmware's first frame.
        #[arg(long)]
        boot: bool,
        /// How long to record.
        #[arg(long, default_value_t = 120)]
        seconds: u32,
        /// Milliseconds between frames; frame n is taken n times this after the start.
        #[arg(long, default_value_t = 100)]
        every_ms: u32,
    },
    /// Wait for the VM's recording to end and copy its frames into <dir>/frames.
    Frames { host: String, dir: PathBuf },
    /// Type text on the VM's keyboard, through a US layout.
    Type {
        host: String,
        text: String,
        /// Press Return after the text.
        #[arg(long)]
        enter: bool,
    },
    /// Press key combinations on the VM's keyboard in turn, named as QEMU's qcodes: ret, tab,
    /// shift-tab, ctrl-alt-f2.
    Key {
        host: String,
        #[arg(required = true)]
        keys: Vec<String>,
    },
    /// Show whether each VM host the spec declares, or each one named, is running.
    Status { hosts: Vec<String> },
    /// Open a shell on the VM over its ssh alias, or run `-- <command>` there.
    Ssh {
        host: String,
        #[arg(last = true)]
        command: Vec<String>,
    },
}

pub fn run(spec: &fspec_eval::Spec, cmd: &VmCommand) -> Result<()> {
    let runner = LiveRunner;
    match cmd {
        VmCommand::Start { host } => start(&runner, spec, host, None),
        VmCommand::Record { host, boot: true, seconds, every_ms } => {
            start(&runner, spec, host, Some(&Record { every_ms: *every_ms, seconds: *seconds }))
        }
        VmCommand::Record { host, boot: false, seconds, every_ms } => {
            let (host, vm) = declared(spec, host)?;
            let script = fspec_try::vm::record(&host.name, &Record { every_ms: *every_ms, seconds: *seconds })?;
            run_script(&runner, machine(spec, &vm.on).as_deref(), &format!("record-{}", host.name), &script)
        }
        VmCommand::Frames { host, dir } => {
            let (host, vm) = declared(spec, host)?;
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
            let script = fspec_try::vm::frames(&host.name)?;
            let from = match machine(spec, &vm.on) {
                Some(alias) => batch(&alias).arg("bash -s"),
                None => Cmd::new("bash").arg("-s"),
            };
            let script_file = tempfile::NamedTempFile::new()?;
            std::fs::write(script_file.path(), script)?;
            // The tar goes straight from the pipe to disk; a recording is too large to hold.
            let pipe = Cmd::new("sh")
                .args(["-c", "f=$1 to=$2; shift 2; \"$@\" <\"$f\" | tar -C \"$to\" -xf -", "sh"])
                .arg(script_file.path().display().to_string())
                .arg(dir.display().to_string())
                .arg(from.program)
                .args(from.args)
                .interactive();
            let out = runner.run(&pipe)?;
            if out.status != 0 {
                std::process::exit(out.status);
            }
            let n = std::fs::read_dir(dir.join("frames")).map(|d| d.count()).unwrap_or(0);
            println!("{n} frames in {}", dir.join("frames").display());
            Ok(())
        }
        VmCommand::Type { host, text, enter } => {
            let mut presses = fspec_try::vm::text_keys(text)?;
            if *enter {
                presses.push(vec!["ret".into()]);
            }
            keys(&runner, spec, host, &presses)
        }
        VmCommand::Key { host, keys: combos } => {
            let presses = combos.iter().map(|c| fspec_try::vm::combo_keys(c)).collect::<Result<Vec<_>>>()?;
            keys(&runner, spec, host, &presses)
        }
        VmCommand::Stop { host } => {
            let (host, vm) = declared(spec, host)?;
            let script = fspec_try::vm::stop(&host.name)?;
            run_script(&runner, machine(spec, &vm.on).as_deref(), &format!("stop-{}", host.name), &script)
        }
        VmCommand::Status { hosts } => {
            let mut vms: Vec<(&Host, &Vm)> = match hosts.as_slice() {
                [] => spec.hosts.iter().filter_map(|h| h.vm.as_ref().map(|v| (h, v))).collect(),
                names => names.iter().map(|n| declared(spec, n)).collect::<Result<_>>()?,
            };
            vms.sort_by(|a, b| (&a.1.on, &a.0.name).cmp(&(&b.1.on, &b.0.name)));
            for on in vms.iter().map(|(_, v)| v.on.as_str()).collect::<std::collections::BTreeSet<_>>() {
                let here: Vec<(&str, u16)> =
                    vms.iter().filter(|(_, v)| v.on == on).map(|(h, v)| (h.name.as_str(), v.port)).collect();
                let script = fspec_try::vm::status(&here)?;
                let cmd = match machine(spec, on) {
                    Some(alias) => batch(&alias).arg("bash -s").stdin(script),
                    None => Cmd::new("bash").arg("-s").stdin(script),
                };
                for line in runner.check(&cmd)?.stdout.lines() {
                    let mut f = line.split('\t');
                    let (name, state, port) = (f.next().unwrap_or(""), f.next().unwrap_or(""), f.next().unwrap_or(""));
                    println!("{name:<16}{state:<9}on {on}, ssh 127.0.0.1:{port}");
                }
            }
            Ok(())
        }
        VmCommand::Ssh { host, command } => {
            let (host, _) = declared(spec, host)?;
            let alias = host.ssh.clone().unwrap_or_else(|| host.name.clone());
            let out = runner.run(&Cmd::new("ssh").arg(alias).args(command).interactive())?;
            if out.status != 0 {
                std::process::exit(out.status);
            }
            Ok(())
        }
    }
}

fn start(runner: &dyn Runner, spec: &fspec_eval::Spec, name: &str, record: Option<&Record>) -> Result<()> {
    let (host, vm) = declared(spec, name)?;
    let distro = match &host.system {
        Some(System::Distro(d)) => d,
        _ => bail!("host {} needs a distro system for fspec vm to boot its cloud image", host.name),
    };
    let user = host.users.first().with_context(|| format!("host {} declares no user to log in as", host.name))?;
    // A Tumbleweed VM boots the way a root `switch --beside tumbleweed` installed does, under the
    // same name on the ESP, so a spec's boot chain (Limine, plymouth, a `preferred` entry) finds
    // what it builds on.
    let (files, first_boot) = match distro.distro.as_str() {
        "opensuse-tumbleweed" => {
            let (files, script) = fspec_switch::tumbleweed_vm_boot("tumbleweed", &distro.kernel_args);
            (files, Some(script))
        }
        _ => (vec![], None),
    };
    let guest = Guest { name: host.name.clone(), user: user.name.clone(), uid: user.uid, files, first_boot };
    let url = fspec_try::vm::image_url(&distro.distro, vm)?;
    let mut image = Iso::from_url(&url)?;
    // Only a new disk downloads the image, so a missing checksum list fails there, not here.
    image.sha256 = LiveFetch::new()
        .get(&format!("{url}.sha256"))
        .ok()
        .and_then(|b| fspec_try::vm::image_sum(&String::from_utf8_lossy(&b), &image));
    let script = fspec_try::vm::start(&guest, vm, &image, record)?;
    run_script(runner, machine(spec, &vm.on).as_deref(), &format!("start-{}", host.name), &script)
}

fn keys(runner: &dyn Runner, spec: &fspec_eval::Spec, name: &str, presses: &[Vec<String>]) -> Result<()> {
    let (host, vm) = declared(spec, name)?;
    let script = fspec_try::vm::send_keys(&host.name, presses)?;
    run_script(runner, machine(spec, &vm.on).as_deref(), &format!("keys-{}", host.name), &script)
}

fn declared<'s>(spec: &'s fspec_eval::Spec, name: &str) -> Result<(&'s Host, &'s Vm)> {
    let host = spec.host(name)?;
    let vm = host.vm.as_ref().with_context(|| format!("host {name} declares no vm(...)"))?;
    Ok((host, vm))
}

/// The ssh alias of the machine a VM runs on, or `None` for this machine. A machine the spec
/// declares is reached as its `ssh` field says.
fn machine(spec: &fspec_eval::Spec, on: &str) -> Option<String> {
    if on.eq_ignore_ascii_case(&local_hostname()) {
        return None;
    }
    Some(spec.hosts.iter().find(|h| h.name == on).and_then(|h| h.ssh.clone()).unwrap_or_else(|| on.to_string()))
}

/// Runs `script` with the terminal handed over, so its progress shows as it happens: here, or
/// staged on `alias` under `~/.cache/fspec/vm/<name>.sh` first, since a script fed on stdin
/// leaves the remote no stdin of its own.
fn run_script(runner: &dyn Runner, alias: Option<&str>, name: &str, script: &str) -> Result<()> {
    let cmd = match alias {
        None => Cmd::new("bash").args(["-c", script]).interactive(),
        Some(alias) => {
            runner.check(
                &batch(alias)
                    .arg(format!("mkdir -p \"$HOME/.cache/fspec/vm\" && cat > \"$HOME/.cache/fspec/vm/{name}.sh\""))
                    .stdin(script),
            )?;
            batch(alias).arg(format!("bash \"$HOME/.cache/fspec/vm/{name}.sh\"")).interactive()
        }
    };
    let out = runner.run(&cmd)?;
    if out.status != 0 {
        std::process::exit(out.status);
    }
    Ok(())
}
