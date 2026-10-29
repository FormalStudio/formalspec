use std::path::PathBuf;
use std::process::{Command, exit};
use std::sync::Arc;

use anyhow::{Result, bail};
use clap::Parser;
use fspec_install::answers::{self, Answers};
use fspec_install::backend::Backend;
use fspec_install::demo::DemoBackend;
use fspec_install::keys;
use fspec_install::live::Live;
use fspec_install::ui::{self, Exit};

#[derive(Parser)]
#[command(version, about = "Install a Linux system that FormalSpec manages from its first boot")]
struct Args {
    /// Run against a made-up machine; nothing is written anywhere.
    #[arg(long)]
    demo: bool,
    /// Install without the screens, with the choices in this JSON file.
    #[arg(long, value_name = "FILE", conflicts_with = "demo")]
    answers: Option<PathBuf>,
}

fn main() -> Result<()> {
    if let Some(password) = std::env::var_os(keys::ASKPASS_ENV) {
        println!("{}", password.to_string_lossy());
        return Ok(());
    }
    let args = Args::parse();
    if let Some(file) = &args.answers {
        if !is_root() {
            bail!("the installer partitions disks, so it runs as root");
        }
        let answers = Answers::parse(&std::fs::read_to_string(file)?)?;
        let mut choices = answers.choices()?;
        for source in answers.key_sources()? {
            choices.keys.push(Live::new().fetch_key(&source)?);
        }
        let generated: Vec<_> = choices.keys.iter().filter(|k| k.generated).cloned().collect();
        let host = choices.account.hostname.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || Live::new().install(&choices, tx));
        answers::report(rx, &mut std::io::stdout())?;
        for line in generated.iter().flat_map(|k| keys::rekey_steps(&host, k)) {
            println!("fspec-install: {line}");
        }
        return Ok(());
    }
    let (backend, checkout): (Arc<dyn Backend>, PathBuf) = if args.demo {
        (Arc::new(DemoBackend::new()), std::env::temp_dir().join("fspec-install-demo"))
    } else {
        if !is_root() {
            bail!("the installer partitions disks, so it runs as root");
        }
        (Arc::new(Live::new()), PathBuf::from("/run/fspec-install/clone"))
    };
    match ui::run(backend, checkout)? {
        Exit::Restart => {
            Command::new("systemctl").arg("reboot").status()?;
        }
        Exit::Shell => exit(2),
        Exit::Quit => {}
    }
    Ok(())
}

fn is_root() -> bool {
    Command::new("id").arg("-u").output().is_ok_and(|o| o.stdout.trim_ascii() == b"0")
}
