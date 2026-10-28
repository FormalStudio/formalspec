//! `fspec try`: the CLI end of [`fspec_try`]. It resolves the ISO here, then stages and runs the
//! boot script on the VM's host, and for a remote host tunnels the VNC socket back over ssh and
//! opens a viewer once the VM answers on it.

use std::io::Read;
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use fspec_core::facts::local_hostname;
use fspec_core::fetch::LiveFetch;
use fspec_core::runner::{Cmd, LiveRunner, Runner};
use fspec_try::{Display, Iso, Media, Target, Vm};

pub struct Args<'a> {
    pub target: Option<&'a str>,
    pub host: Option<&'a str>,
    pub keep: Option<&'a str>,
    pub cpus: Option<u32>,
    pub memory: Option<u32>,
    pub disk: u32,
    pub headless: bool,
    pub print: bool,
    pub list: bool,
    pub delete: Option<&'a str>,
}

pub fn run(a: &Args) -> Result<()> {
    let runner = LiveRunner;
    let remote = a.host.filter(|h| !h.eq_ignore_ascii_case(&local_hostname()));
    let on = |script: &str| -> Cmd {
        match remote {
            Some(alias) => fspec_remote::cmd::batch(alias).arg("bash -s").stdin(script),
            None => Cmd::new("bash").arg("-s").stdin(script),
        }
    };
    if a.list {
        println!("distros:");
        for (name, what) in fspec_try::DISTROS {
            println!("  {name:<12}{what}");
        }
        print!("{}", runner.check(&on(fspec_try::LIST))?.stdout);
        return Ok(());
    }
    if let Some(name) = a.delete {
        runner.check(&on(&fspec_try::delete(name)?))?;
        println!("deleted {name}");
        return Ok(());
    }
    if let Some(name) = a.keep
        && !fspec_try::valid_name(name)
    {
        bail!("`{name}` is not a VM name: use letters, digits, `-`, `_` and `.`");
    }
    let (media, label) = match a.target.map(fspec_try::target).transpose()? {
        Some(Target::Distro(d)) => {
            let iso =
                fspec_try::resolve(&LiveFetch::new(), d).with_context(|| format!("finding the current {d} ISO"))?;
            (Media::Download(iso), d.to_string())
        }
        Some(Target::Url(url)) => {
            let iso = Iso::from_url(&url)?;
            let label = iso.file.trim_end_matches(".iso").to_string();
            (Media::Download(iso), label)
        }
        Some(Target::Path(p)) => {
            let p = match remote {
                Some(_) => p,
                None => std::path::absolute(&p)?.display().to_string(),
            };
            let label = p.rsplit('/').next().unwrap_or(&p).trim_end_matches(".iso").to_string();
            (Media::Path(p), label)
        }
        None => match a.keep {
            Some(name) => (Media::None, name.to_string()),
            None => bail!("name a distro, ISO URL or path to try, or a VM to boot again with --keep"),
        },
    };
    let windowed = std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some();
    let display = if remote.is_none() && windowed && !a.headless { Display::Window } else { Display::Vnc };
    let random = random_hex(16)?;
    let vm = Vm {
        id: random[..8].to_string(),
        label: a.keep.map_or(label, String::from),
        media,
        keep: a.keep.map(String::from),
        cpus: a.cpus,
        memory_gib: a.memory,
        disk_gib: a.disk,
        display,
        password: random[8..].to_string(),
    };
    let script = fspec_try::script(&vm);
    if a.print {
        print!("{script}");
        return Ok(());
    }
    if remote.is_none() && std::env::consts::OS != "linux" {
        bail!("fspec try runs VMs on Linux hosts with KVM; pass --host <a Linux host> to run it there");
    }
    runner.check(&on_stage(remote, &vm.id).stdin(script))?;
    let Some(alias) = remote else {
        if display == Display::Vnc {
            eprintln!("fspec try: no window; VNC on {} with password {}", fspec_try::vnc_socket(&vm.id), vm.password);
        }
        runner.run(&Cmd::new("bash").arg(fspec_try::script_path(&vm.id)).interactive())?;
        return Ok(());
    };
    let port = free_port()?;
    let ssh = fspec_try::remote_boot(alias, &vm.id, port);
    let url = format!("vnc://:{}@127.0.0.1:{port}", vm.password);
    let viewer = (!a.headless).then(|| fspec_try::viewer(std::env::consts::OS, port, &vm.password, on_path)).flatten();
    if viewer.is_none() {
        eprintln!("fspec try: the VM's display is at {url} once it boots");
    }
    let done = AtomicBool::new(false);
    std::thread::scope(|s| {
        if let Some(viewer) = &viewer {
            s.spawn(|| {
                if wait_for_vnc(port, &done)
                    && let Err(e) = runner.check(viewer)
                {
                    eprintln!("fspec try: could not open the viewer ({e:#}); connect to {url}");
                }
            });
        }
        let out = runner.run(&ssh);
        done.store(true, Ordering::Relaxed);
        out
    })?;
    Ok(())
}

fn on_stage(remote: Option<&str>, id: &str) -> Cmd {
    let stage = fspec_try::stage(id);
    match remote {
        Some(alias) => fspec_remote::cmd::batch(alias).arg(stage),
        None => Cmd::new("sh").args(["-c", &stage]),
    }
}

/// Polls the tunnel until qemu's VNC server greets through it. ssh accepts on the local port at
/// once and only then finds out whether the remote socket exists, so a connect alone proves nothing.
fn wait_for_vnc(port: u16, done: &AtomicBool) -> bool {
    while !done.load(Ordering::Relaxed) {
        if let Ok(mut c) = TcpStream::connect((Ipv4Addr::LOCALHOST, port)) {
            let _ = c.set_read_timeout(Some(Duration::from_secs(2)));
            let mut hello = [0u8; 4];
            if c.read_exact(&mut hello).is_ok() && &hello == b"RFB " {
                return true;
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    false
}

fn free_port() -> Result<u16> {
    Ok(TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?.local_addr()?.port())
}

fn random_hex(n: usize) -> Result<String> {
    let mut bytes = vec![0u8; n.div_ceil(2)];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok(hex[..n].to_string())
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(program).is_file()))
}
