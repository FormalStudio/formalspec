//! `fspec vm`: the CLI end of [`fspec_try::vm`]. It reads the host's `vm(...)` from the spec,
//! resolves the cloud image here, and runs the script on the machine the VM lives on.

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use fspec_core::Host;
use fspec_core::facts::local_hostname;
use fspec_core::fetch::{Fetch, LiveFetch};
use fspec_core::resource::{System, Vm};
use fspec_core::runner::{Cmd, LiveRunner, Runner};
use fspec_remote::cmd::batch;
use fspec_try::Iso;
use fspec_try::vm::Guest;

#[derive(Subcommand)]
pub enum VmCommand {
    /// Boot the host's VM where its vm(on = ...) says, creating its disk on the first start, and
    /// wait until its sshd answers.
    Start { host: String },
    /// Power the host's VM off; its disk stays.
    Stop { host: String },
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
        VmCommand::Start { host } => {
            let (host, vm) = declared(spec, host)?;
            let distro = match &host.system {
                Some(System::Distro(d)) => d.distro.as_str(),
                _ => bail!("host {} needs a distro system for fspec vm to boot its cloud image", host.name),
            };
            let user =
                host.users.first().with_context(|| format!("host {} declares no user to log in as", host.name))?;
            let guest = Guest { name: host.name.clone(), user: user.name.clone(), uid: user.uid };
            let url = fspec_try::vm::image_url(distro, vm)?;
            let mut image = Iso::from_url(&url)?;
            // Only a new disk downloads the image, so a missing checksum list fails there, not here.
            image.sha256 = LiveFetch::new()
                .get(&format!("{url}.sha256"))
                .ok()
                .and_then(|b| fspec_try::vm::image_sum(&String::from_utf8_lossy(&b), &image));
            let script = fspec_try::vm::start(&guest, vm, &image)?;
            run_script(&runner, machine(spec, &vm.on).as_deref(), &format!("start-{}", host.name), &script)
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
