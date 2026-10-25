//! The install: `disk init` with the chosen distro as its first root, plus the steps only the
//! installer takes inside that root, run with each command's output streamed to the front end.

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::mpsc::Sender;

use anyhow::{Context, Result, bail};
use fspec_core::Paths;
use fspec_core::resource::System;
use fspec_core::runner::{Cmd, Output, Runner};
use fspec_switch::{Init, Kind, Plan, Step};

use crate::catalog;
use crate::model::{Choices, Progress};
use crate::secure_boot;

/// Everything `plan` needs besides the choices, read from the live session.
pub struct Setup<'a> {
    /// The spec repo to install, already written or cloned.
    pub spec_dir: &'a Path,
    pub host: &'a str,
    /// The static `fspec` binary the new root runs its first apply with.
    pub fspec: &'a Path,
    pub efi: bool,
    /// The firmware is in Secure Boot setup mode, the only state sbctl can enrol keys from.
    pub setup_mode: bool,
    /// The live session's kernel command line; only its `console=` options carry over.
    pub cmdline: &'a str,
    /// Fedora's release number, read from Fedora's release list at install time.
    pub fedora_release: Option<&'a str>,
    pub ids: &'a fspec_switch::Ids,
}

pub fn plan(paths: &Paths, c: &Choices, s: &Setup) -> Result<Plan> {
    let spec = fspec_eval::evaluate(&s.spec_dir.join("spec.star"))?;
    let host = spec.host(s.host)?;
    let distro = match &host.system {
        Some(System::Distro(d)) => d.distro.as_str(),
        Some(System::Image(i)) => bail!("{} is a bootc image host; the installer cannot install those yet", i.distro()),
        None => bail!("host {} declares no system", host.name),
    };
    let user = host.users.first().with_context(|| format!("host {} declares no user", host.name))?;
    let release = if distro == "fedora" { s.fedora_release } else { None };
    let target = fspec_switch::target(host, distro, release, s.spec_dir, "spec.star", s.fspec)?;

    let root = fspec_switch::root_dir(paths, distro);
    let home = format!("/home/{}/spec", user.name);
    // debootstrap copies the live system's /etc/hostname into the new root.
    let in_root = [
        Cmd::new("tee").arg(format!("{root}/etc/hostname")).stdin(format!("{}\n", host.name)),
        Cmd::new("tee").arg("-a").arg(format!("{root}/etc/hosts")).stdin(format!("127.0.1.1 {}\n", host.name)),
        Cmd::new("chroot").arg(&root).arg("chpasswd").stdin(format!("{}:{}\n", user.name, c.account.password.0)),
        Cmd::new("cp").arg("-a").arg(s.spec_dir.display().to_string()).arg(format!("{root}{home}")),
        Cmd::new("chroot").arg(&root).args(["chown", "-R"]).arg(format!("{0}:{0}", user.name)).arg(&home),
    ];
    let console: Vec<&str> = s.cmdline.split_whitespace().filter(|a| a.starts_with("console=")).collect();
    let console = console.join(" ");
    let init = Init {
        device: &c.disk.device,
        kind: if s.efi { Kind::Default } else { Kind::Bios },
        luks: c.disk.encrypt.is_some(),
        ids: s.ids,
        swap_gib: c.disk.swap_gib,
        efi_vars: s.efi,
        target: Some(&target),
        cmdline: &console,
        in_root: &in_root,
        passphrase: c.disk.encrypt.as_ref().map(|p| p.0.as_str()),
        windows: c.disk.windows.as_ref(),
    };
    let mut plan = fspec_switch::init(paths, &init)?;
    if c.secure_boot && s.efi && s.setup_mode {
        let disk = Path::new(&root).parent().map_or_else(String::new, |p| p.display().to_string());
        let loader = c.disk.windows.is_some().then(|| format!("{disk}/esp"));
        let cmds = secure_boot::steps(distro, &root, &format!("{disk}/efi"), loader.as_deref());
        plan.steps.extend(cmds.into_iter().map(Step::Run));
    }
    Ok(plan)
}

/// The stage a command belongs to, or `None` for setup commands like `mount` that continue the
/// stage before them.
fn stage_of(cmd: &Cmd, device: &str, distro: &str, luks: bool) -> Option<String> {
    let arg = |a: &str| cmd.args.iter().any(|x| x == a);
    let name = catalog::distro(distro).map_or(distro, |d| d.name);
    Some(match cmd.program.as_str() {
        "ntfsresize" | "sfdisk" => "Shrinking Windows".into(),
        "wipefs" | "sgdisk" => format!("Partitioning {device}"),
        "cryptsetup" if arg("luksFormat") => "Encrypting and formatting".into(),
        p if p.starts_with("mkfs") && luks => "Encrypting and formatting".into(),
        p if p.starts_with("mkfs") => "Formatting".into(),
        "btrfs" if arg("subvolume") || arg("mkswapfile") => "Creating subvolumes".into(),
        "bootctl" | "grub-install" | "efibootmgr" => "Installing the bootloader".into(),
        "dnf" | "pacstrap" | "debootstrap" | "xbps-install" => format!("Installing {name}"),
        "sbctl" => "Setting up Secure Boot".into(),
        "chroot" if cmd.args.iter().any(|a| a.contains("sbctl")) => "Setting up Secure Boot".into(),
        "chroot" if arg("apply") => "Applying your spec".into(),
        "chroot" if arg("chpasswd") => "Setting your password".into(),
        // The script that moves the copied fspec binary into place is setup, not the kernel step.
        "chroot" if cmd.args.iter().any(|a| a.contains("fspec.new")) => return None,
        "chroot" if arg("-c") => "Building the kernel image".into(),
        "cp" if cmd.args.iter().any(|a| a.contains("/home/")) => "Copying your spec".into(),
        _ => return None,
    })
}

/// The stage titles of `plan` in the order they start, and the stage of each step. A command whose
/// stage already ran, like the new root's own subvolume, stays in the current one.
fn stages(plan: &Plan, device: &str, distro: &str) -> (Vec<String>, Vec<usize>) {
    let luks = plan.lines().iter().any(|l| l.starts_with("cryptsetup luksFormat"));
    let mut titles: Vec<String> = Vec::new();
    let mut of_step = Vec::new();
    for step in &plan.steps {
        let Step::Run(cmd) = step else {
            of_step.push(titles.len().saturating_sub(1));
            continue;
        };
        if let Some(t) = stage_of(cmd, device, distro, luks)
            && !titles.contains(&t)
        {
            titles.push(t);
        }
        of_step.push(titles.len().saturating_sub(1));
    }
    titles.push("Finishing".into());
    (titles, of_step)
}

/// Execute `plan`, reporting each stage, every output line and a failure on `tx` and appending all
/// of it to `log`. Cleanup runs whether or not the steps succeeded, as [`Plan::execute`] does.
pub fn execute(plan: &Plan, device: &str, distro: &str, log: &Path, tx: &Sender<Progress>) -> Result<()> {
    let runner = Streaming { tx: tx.clone(), log: Mutex::new(File::create(log)?) };
    execute_with(plan, &runner, device, distro, log, tx)
}

/// [`execute`] on `runner`. The undo steps reached run, newest first, when a later step fails,
/// after the cleanup has unmounted what they would undo.
fn execute_with(
    plan: &Plan,
    runner: &dyn Runner,
    device: &str,
    distro: &str,
    log: &Path,
    tx: &Sender<Progress>,
) -> Result<()> {
    let (titles, of_step) = stages(plan, device, distro);
    let total = titles.len();
    let mut current = usize::MAX;
    let mut undo = Vec::new();
    let mut result = Ok(());
    for (step, &stage) in plan.steps.iter().zip(&of_step) {
        if stage != current {
            current = stage;
            let _ = tx.send(Progress::Stage { index: stage, total, title: titles[stage].clone() });
        }
        if let Step::Undo(c) = step {
            undo.push(c);
            continue;
        }
        let one = Plan { steps: vec![step.clone()], cleanup: Vec::new() };
        if let Err(e) = one.execute(runner) {
            let error = format!("{e:#}");
            let _ = tx.send(Progress::Failed { stage: titles[stage].clone(), error, log: log.to_path_buf() });
            result = Err(e);
            break;
        }
    }
    if result.is_ok() {
        let _ = tx.send(Progress::Stage { index: total - 1, total, title: titles[total - 1].clone() });
    }
    for c in &plan.cleanup {
        let _ = runner.run(c);
    }
    if result.is_err() {
        for c in undo.iter().rev() {
            let _ = runner.run(c);
        }
    }
    result
}

struct Streaming {
    tx: Sender<Progress>,
    log: Mutex<File>,
}

impl Streaming {
    fn line(&self, line: &str) {
        if let Ok(mut f) = self.log.lock() {
            let _ = writeln!(f, "{line}");
        }
        let _ = self.tx.send(Progress::Output(line.to_string()));
    }
}

impl Runner for Streaming {
    fn run(&self, cmd: &Cmd) -> Result<Output> {
        self.line(&format!("$ {cmd}"));
        let mut c = Command::new(&cmd.program);
        c.args(&cmd.args).envs(cmd.env.iter().map(|(k, v)| (k, v)));
        c.stdout(Stdio::piped()).stderr(Stdio::piped());
        c.stdin(match (&cmd.stdin_file, &cmd.stdin) {
            (Some(path), _) => File::open(path).with_context(|| format!("opening {}", path.display()))?.into(),
            (None, Some(_)) => Stdio::piped(),
            (None, None) => Stdio::null(),
        });
        let mut child = c.spawn().with_context(|| format!("running `{cmd}`"))?;
        if let (None, Some(input)) = (&cmd.stdin_file, &cmd.stdin) {
            child.stdin.take().context("stdin")?.write_all(input.as_bytes())?;
        }
        let (out, err) = (child.stdout.take().context("stdout")?, child.stderr.take().context("stderr")?);
        let (stdout, stderr) = std::thread::scope(|s| {
            let collect = |r: Box<dyn std::io::Read + Send>| {
                let mut all = String::new();
                for line in BufReader::new(r).lines().map_while(Result::ok) {
                    self.line(&line);
                    all.push_str(&line);
                    all.push('\n');
                }
                all
            };
            let e = s.spawn(move || collect(Box::new(err)));
            let o = collect(Box::new(out));
            (o, e.join().unwrap_or_default())
        });
        let status = child.wait()?.code().unwrap_or(-1);
        Ok(Output { status, stdout, stderr })
    }
}

/// The newest stable Fedora release from Fedora's own list.
pub fn fedora_release(releases_json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(releases_json).ok()?;
    v.as_array()?.iter().filter_map(|r| r["version"].as_str()?.parse::<u32>().ok()).max().map(|n| n.to_string())
}

/// Where the live session keeps the install log.
pub fn log_path() -> PathBuf {
    PathBuf::from("/var/log/fspec-install.log")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Account, DiskChoice, Locale, Preset, Secret, SystemChoice};

    fn choices(encrypt: bool) -> Choices {
        Choices {
            locale: Locale { locale: "en_US.UTF-8".into(), keymap: "us".into(), timezone: "UTC".into() },
            disk: DiskChoice {
                device: "/dev/vdb".into(),
                encrypt: encrypt.then(|| Secret("correct horse".into())),
                swap_gib: 2,
                windows: None,
            },
            system: SystemChoice::Fresh { distro: "arch", preset: Preset::Minimal, desktop: None },
            account: Account {
                name: "kyan".into(),
                full_name: "Kyan".into(),
                hostname: "nova".into(),
                password: Secret("hunter22".into()),
            },
            secure_boot: false,
        }
    }

    fn secure(mut c: Choices) -> Choices {
        c.secure_boot = true;
        c
    }

    fn spec_dir() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("spec.star"),
            "hosts = [host(\"nova\", platform = \"x86_64-linux\", system = arch(packages = [\"sudo\"]), \
             users = [user(\"kyan\", uid = 1000)])]\n",
        )
        .unwrap();
        d
    }

    fn ids() -> fspec_switch::Ids {
        fspec_switch::Ids { fs: "ffff".into(), luks: "llll".into(), serial: "ABCD1234".into(), boot: "bbbb".into() }
    }

    fn setup<'a>(dir: &'a Path, ids: &'a fspec_switch::Ids, efi: bool) -> Setup<'a> {
        Setup {
            spec_dir: dir,
            host: "nova",
            fspec: Path::new("/usr/local/bin/fspec"),
            efi,
            setup_mode: true,
            cmdline: "BOOT_IMAGE=/live/vmlinuz boot=live components quiet console=ttyS0,115200",
            fedora_release: Some("44"),
            ids,
        }
    }

    #[test]
    fn installs_onto_the_default_layout_and_sets_the_password_inside_the_new_root() {
        let dir = spec_dir();
        let ids = ids();
        let paths = Paths::new("/r".into(), "/r/root".into());
        let plan = plan(&paths, &choices(true), &setup(dir.path(), &ids, true)).unwrap();
        let lines = plan.lines();
        let at = |p: &str| lines.iter().position(|l| l.starts_with(p)).unwrap_or_else(|| panic!("no `{p}`"));
        assert!(lines.iter().any(|l| l.starts_with("sgdisk -n1:0:+2G -t1:ef00")));
        assert!(at("pacstrap -K /r/run/fspec/disk/arch") < at("chroot /r/run/fspec/disk/arch chpasswd"));
        assert!(at("cp -a") < at("chroot /r/run/fspec/disk/arch chown -R kyan:kyan /home/kyan/spec"));
        assert!(!lines.iter().any(|l| l.contains("hunter22") || l.contains("correct horse")));
        let kernel = plan.steps.iter().find_map(|s| match s {
            Step::Run(c) if c.args.iter().any(|a| a.ends_with("/etc/kernel/cmdline")) => c.stdin.clone(),
            _ => None,
        });
        let kernel = kernel.unwrap();
        assert!(kernel.contains("console=ttyS0,115200") && !kernel.contains("boot=live"), "{kernel}");
    }

    #[test]
    fn secure_boot_signs_the_boot_files_and_enrols_the_keys_last() {
        let dir = spec_dir();
        let ids = ids();
        let paths = Paths::new("/r".into(), "/r/root".into());
        let plan = plan(&paths, &secure(choices(false)), &setup(dir.path(), &ids, true)).unwrap();
        let lines = plan.lines();
        let at = |p: &str| lines.iter().position(|l| l.starts_with(p)).unwrap_or_else(|| panic!("no `{p}`"));
        let root = "chroot /r/run/fspec/disk/arch";
        let sign =
            lines.iter().position(|l| l.starts_with(root) && l.contains("sbctl sign -s /efi/EFI/systemd")).unwrap();
        assert!(at("chroot /r/run/fspec/disk/arch chpasswd") < at("sbctl create-keys"));
        assert!(at("sbctl create-keys") < at(&format!("{root} pacman -S --noconfirm --needed sbctl")));
        assert!(at("cp -a /var/lib/sbctl /r/run/fspec/disk/arch/var/lib/sbctl") < sign);
        assert!(lines.iter().any(|l| l.contains("sbctl sign -s /efi/EFI/systemd/systemd-bootx64.efi")));
        assert!(lines.iter().any(|l| l.starts_with("tee /r/run/fspec/disk/arch/etc/initcpio/post/sbctl")));
        let enrol = at("sbctl enroll-keys --microsoft");
        assert!(sign < enrol);
        assert!(lines[enrol + 1..].iter().all(|l| l.starts_with("umount")), "{lines:#?}");
        let (titles, _) = stages(&plan, "/dev/vdb", "arch");
        assert_eq!(titles[titles.len() - 2], "Setting up Secure Boot");
    }

    #[test]
    fn secure_boot_off_or_outside_setup_mode_runs_no_sbctl() {
        let dir = spec_dir();
        let ids = ids();
        let paths = Paths::new("/r".into(), "/r/root".into());
        let none = |p: &fspec_switch::Plan| !p.lines().iter().any(|l| l.contains("sbctl"));
        assert!(none(&plan(&paths, &choices(false), &setup(dir.path(), &ids, true)).unwrap()));
        let mut s = setup(dir.path(), &ids, true);
        s.setup_mode = false;
        assert!(none(&plan(&paths, &secure(choices(false)), &s).unwrap()));
        assert!(none(&plan(&paths, &secure(choices(false)), &setup(dir.path(), &ids, false)).unwrap()));
    }

    #[test]
    fn a_machine_without_efi_gets_the_bios_layout() {
        let dir = spec_dir();
        let ids = ids();
        let paths = Paths::new("/r".into(), "/r/root".into());
        let plan = plan(&paths, &choices(false), &setup(dir.path(), &ids, false)).unwrap();
        assert!(plan.lines().iter().any(|l| l.starts_with("grub-install --target=i386-pc")));
    }

    #[test]
    fn names_the_stages_in_order() {
        let dir = spec_dir();
        let ids = ids();
        let paths = Paths::new("/r".into(), "/r/root".into());
        let plan = plan(&paths, &choices(true), &setup(dir.path(), &ids, true)).unwrap();
        let (titles, of_step) = stages(&plan, "/dev/vdb", "arch");
        assert_eq!(
            titles,
            [
                "Partitioning /dev/vdb",
                "Encrypting and formatting",
                "Creating subvolumes",
                "Installing the bootloader",
                "Installing Arch Linux",
                "Applying your spec",
                "Building the kernel image",
                "Setting your password",
                "Copying your spec",
                "Finishing"
            ]
        );
        assert_eq!(of_step.len(), plan.steps.len());
        assert!(of_step.windows(2).all(|w| w[0] <= w[1]));
    }

    fn beside_windows() -> Choices {
        let part = |dev: &str, partn, start, size, fstype: &str| fspec_switch::Part {
            dev: dev.into(),
            partn,
            start,
            size,
            fstype: fstype.into(),
            ..fspec_switch::Part::default()
        };
        let disk = fspec_switch::WindowsDisk {
            disk: "/dev/nvme0n1".into(),
            sector: 512,
            esp: part("/dev/nvme0n1p1", 1, 2048, 100 << 20, "vfat"),
            windows: part("/dev/nvme0n1p3", 3, 239616, 100 * fspec_switch::GIB, "ntfs"),
            windows_limit: 239616 + (100 << 21),
            gaps: vec![fspec_switch::Gap { start: 210_000_000, end: 290_000_000 }],
            next_partn: 5,
            shrink_min: Ok(fspec_switch::GIB),
        };
        let space = fspec_switch::space(&disk, None).unwrap();
        let mut c = choices(false);
        c.disk.device = "/dev/nvme0n1".into();
        c.disk.windows = Some(fspec_switch::BesideWindows { disk, space });
        c
    }

    #[test]
    fn a_failed_install_beside_windows_removes_the_partitions_it_made() {
        let dir = spec_dir();
        let ids = ids();
        let paths = Paths::new("/r".into(), "/r/root".into());
        let plan = plan(&paths, &beside_windows(), &setup(dir.path(), &ids, true)).unwrap();
        let log = dir.path().join("install.log");
        let remove = r#"sh -c 'sgdisk -d"$2" -d"$3" "$1" && partx -u "$1"' sh /dev/nvme0n1 5 6"#;

        let (tx, _rx) = std::sync::mpsc::channel();
        let missing = Output { status: 1, ..Output::default() };
        let ok = fspec_core::runner::RecordingRunner::default();
        ok.respond("test -e", missing.clone());
        execute_with(&plan, &ok, "/dev/nvme0n1", "arch", &log, &tx).unwrap();
        assert!(!ok.lines().iter().any(|l| l.starts_with("sh -c sgdisk -d")), "{:#?}", ok.lines());

        let failing = fspec_core::runner::RecordingRunner::default();
        failing.respond("test -e", missing);
        failing.respond("pacstrap", Output { status: 1, stdout: String::new(), stderr: "no repositories".into() });
        assert!(execute_with(&plan, &failing, "/dev/nvme0n1", "arch", &log, &tx).is_err());
        let lines = failing.lines();
        let at = |p: &str| lines.iter().rposition(|l| l.starts_with(p)).unwrap_or_else(|| panic!("no `{p}`"));
        assert_eq!(lines.last().map(String::as_str), Some(remove));
        assert!(at("pacstrap") < at("umount /r/run/fspec/disk/esp") && at("umount /r/run/fspec/disk") < at(remove));
    }

    #[test]
    fn picks_the_newest_numbered_fedora_release() {
        let json = r#"[{"version":"45 Beta","variant":"Everything"},{"version":"44","variant":"Everything"},
                       {"version":"43","variant":"Server"}]"#;
        assert_eq!(fedora_release(json).as_deref(), Some("44"));
    }
}
