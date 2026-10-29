use fspec_core::fetch::MapFetch;

use super::*;

const H: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn vm(media: Media, keep: Option<&str>, display: Display) -> Vm {
    Vm {
        id: "ab12".into(),
        label: "debian".into(),
        media,
        keep: keep.map(String::from),
        cpus: None,
        memory_gib: None,
        disk_gib: 32,
        display,
        password: "pw".into(),
    }
}

fn debian() -> Media {
    Media::Download(Iso {
        group: "debian".into(),
        url: "https://cdimage.debian.org/x/debian-live-13.7.0-amd64-gnome.iso".into(),
        file: "debian-live-13.7.0-amd64-gnome.iso".into(),
        sha256: Some(H.into()),
    })
}

#[test]
fn targets() {
    assert_eq!(target("tumbleweed").unwrap(), Target::Distro("tumbleweed"));
    assert_eq!(target("aeryn").unwrap(), Target::Distro("aerynos"));
    assert_eq!(target("https://x.org/a.iso").unwrap(), Target::Url("https://x.org/a.iso".into()));
    assert_eq!(target("~/isos/a.iso").unwrap(), Target::Path("~/isos/a.iso".into()));
    assert_eq!(target("pond.iso").unwrap(), Target::Path("pond.iso".into()));
    let err = target("gentoo").unwrap_err().to_string();
    assert!(err.contains("tumbleweed, fedora"), "{err}");
}

#[test]
fn url_names_the_cached_file() {
    let iso = Iso::from_url("https://x.org/dl/pond-0.1.iso?mirror=eu").unwrap();
    assert_eq!((iso.group.as_str(), iso.file.as_str()), ("url", "pond-0.1.iso"));
    assert!(Iso::from_url("https://x.org/").is_err());
}

#[test]
fn newest_by_number() {
    let sums = format!(
        "{H} *ubuntu-26.04-desktop-amd64.iso\n{H} *ubuntu-26.04-live-server-amd64.iso\n\
         {H} *ubuntu-26.04.1-desktop-amd64.iso\n{H} *ubuntu-26.04.10-desktop-amd64.iso\n{H} *ubuntu-26.04.9-desktop-amd64.iso\n"
    );
    assert_eq!(pick(&sums, "ubuntu-", "-desktop-amd64.iso").unwrap(), "ubuntu-26.04.10-desktop-amd64.iso");
}

#[test]
fn resolves_from_checksum_lists() {
    let other = "f".repeat(64);
    let fetch = MapFetch::default()
        .with(&format!("{DEBIAN}/SHA256SUMS"), format!("{other}  debian-live-13.7.0-amd64-kde.iso\n{H}  debian-live-13.7.0-amd64-gnome.iso\n{other}  debian-live-13.7.0-amd64-gnome.iso.log\n"))
        .with(&format!("{VOID}/sha256sum.txt"), format!("SHA256 (void-live-x86_64-musl-20250202-xfce.iso) = {other}\nSHA256 (void-live-x86_64-20250202-xfce.iso) = {H}\n"))
        .with(&format!("{ARCH}/sha256sums.txt"), format!("{H}  archlinux-2026.10.01-x86_64.iso\n{H}  archlinux-x86_64.iso\n"))
        .with(&format!("{TUMBLEWEED}.sha256"), format!("{H}  openSUSE-Tumbleweed-KDE-Live-x86_64-Snapshot20261026-Media.iso\n"))
        .with(UBUNTU_LTS, "Dist: noble\nVersion: 24.04.5 LTS\n\nDist: resolute\nVersion: 26.04.1 LTS\n")
        .with(&format!("{UBUNTU}/resolute/SHA256SUMS"), format!("{H} *ubuntu-26.04.1-desktop-amd64.iso\n"));
    let debian = resolve(&fetch, "debian").unwrap();
    assert_eq!(debian.url, format!("{DEBIAN}/debian-live-13.7.0-amd64-gnome.iso"));
    assert_eq!(debian.sha256.as_deref(), Some(H));
    let void = resolve(&fetch, "void").unwrap();
    assert_eq!((void.file.as_str(), void.sha256.as_deref()), ("void-live-x86_64-20250202-xfce.iso", Some(H)));
    assert_eq!(resolve(&fetch, "arch").unwrap().file, "archlinux-2026.10.01-x86_64.iso");
    let tw = resolve(&fetch, "tumbleweed").unwrap();
    assert_eq!(tw.url, TUMBLEWEED);
    assert_eq!(tw.file, "openSUSE-Tumbleweed-KDE-Live-x86_64-Snapshot20261026-Media.iso");
    assert_eq!(resolve(&fetch, "ubuntu").unwrap().url, format!("{UBUNTU}/resolute/ubuntu-26.04.1-desktop-amd64.iso"));
}

#[test]
fn fedora_takes_the_newest_final_release() {
    let json = format!(
        r#"[{{"version":"45 Beta","arch":"x86_64","variant":"Workstation","link":"https://f/45b.iso","sha256":"{H}"}},
            {{"version":"44","arch":"x86_64","variant":"Workstation","link":"https://f/Fedora-Workstation-Live-44-1.7.x86_64.iso","sha256":"{H}"}},
            {{"version":"44","arch":"aarch64","variant":"Workstation","link":"https://f/arm.iso","sha256":"{H}"}},
            {{"version":"43","arch":"x86_64","variant":"Workstation","link":"https://f/43.iso","sha256":"{H}"}}]"#
    );
    let iso = resolve(&MapFetch::default().with(FEDORA, json), "fedora").unwrap();
    assert_eq!(iso.file, "Fedora-Workstation-Live-44-1.7.x86_64.iso");
    assert_eq!(iso.sha256.as_deref(), Some(H));
}

#[test]
fn aerynos_scrapes_its_download_page() {
    let url = "https://cdn.aerynos.dev/isos/AerynOS-2026.08-GNOME-live-x86_64.iso";
    let page = format!(
        r#"<a href="https://cdn.aerynos.dev/isos/AerynOS-2026.05-GNOME-live-x86_64.iso">old</a><a href="{url}">x</a>"#
    );
    let fetch = MapFetch::default()
        .with(AERYNOS, page)
        .with(&format!("{url}.sha256sum"), format!("{H}  AerynOS-2026.08-GNOME-live-x86_64.iso\n"));
    let iso = resolve(&fetch, "aerynos").unwrap();
    assert_eq!((iso.url.as_str(), iso.sha256.as_deref()), (url, Some(H)));
}

#[test]
fn qemu_boots_the_disk_before_the_iso_under_uefi() {
    let line = qemu(&vm(debian(), None, Display::Vnc)).to_string();
    for part in [
        "-machine q35,accel=kvm -cpu host -smp $cpus -m ${mem}M",
        "if=pflash,format=raw,readonly=on,file=code.fd",
        "-object iothread,id=io0",
        "discard=unmap,cache=$dcache,aio=$aio,file=disk.qcow2",
        "virtio-blk-pci,drive=disk,iothread=io0,bootindex=0",
        "ide-cd,drive=cd,bootindex=1",
        "-nic user,model=virtio-net-pci",
        "-device usb-tablet",
        "-device virtio-vga -display none",
        "-vnc unix:vnc.sock,password-secret=vncpw",
    ] {
        assert!(line.contains(part), "{part} missing from {line}");
    }
    let window = qemu(&vm(Media::None, Some("tw"), Display::Window)).to_string();
    assert!(window.contains("-device virtio-vga-gl -display gtk,gl=on"), "{window}");
    assert!(!window.contains("cdrom.iso") && !window.contains("-vnc"), "{window}");
}

#[test]
fn throwaway_disk_is_deleted_on_exit() {
    let s = script(&vm(debian(), None, Display::Vnc));
    assert!(
        s.contains("rm -rf \"$run\" \"$cache/disks/$id.qcow2\"\n}\ntrap cleanup EXIT\ntrap 'exit 130' INT HUP TERM"),
        "{s}"
    );
    assert!(s.contains("qemu-img create -q -f qcow2 -o nocow=on \"$cache/disks/$id.qcow2\" 32G"), "{s}");
    assert!(s.contains("dcache=unsafe\naio=io_uring\n"), "{s}");
    assert!(
        s.contains("  -drive \"if=none,id=disk,format=qcow2,discard=unmap,cache=$dcache,aio=$aio,file=disk.qcow2\""),
        "{s}"
    );
    assert!(s.contains("cp \"$vars\" \"$run/vars.fd\""), "{s}");
    assert!(s.contains(&format!("echo \"{H}  $iso.part\" | sha256sum -c --status -")), "{s}");
    assert!(
        s.contains("find \"$cache/iso/debian\" -maxdepth 1 -type f ! -name debian-live-13.7.0-amd64-gnome.iso"),
        "{s}"
    );
    assert!(s.contains("printf %s pw > \"$run/vnc.pw\""), "{s}");
    assert!(s.contains("  -m \"${mem}M\" \\\n"), "{s}");
    assert!(s.ends_with("wait \"$q\" || true\nq=\n"), "{s}");
}

#[test]
fn kept_disk_stays() {
    let s = script(&vm(debian(), Some("deb"), Display::Window));
    assert!(s.contains("  rm -rf \"$run\"\n}"), "{s}");
    assert!(!s.contains("$cache/disks/$id.qcow2\" 32G"), "{s}");
    assert!(
        s.contains("[ -f \"$keep/disk.qcow2\" ] || qemu-img create -q -f qcow2 -o nocow=on \"$keep/disk.qcow2\" 32G"),
        "{s}"
    );
    assert!(s.contains("dcache=none\nqemu-img bench -c 1 -t none -f qcow2 \"$run/disk.qcow2\" >/dev/null 2>&1 || dcache=writeback\n"), "{s}");
    assert!(s.contains("ln -s \"$keep/vars.fd\" \"$run/vars.fd\""), "{s}");
    assert!(!s.contains("vnc.pw"), "{s}");
    let back = script(&vm(Media::None, Some("deb"), Display::Window));
    assert!(back.contains("[ -f \"$keep/disk.qcow2\" ] || die \"no kept VM named \"deb\nmkdir -p"), "{back}");
    assert!(!back.contains("$run/cdrom.iso"), "{back}");
}

#[test]
fn fixed_resources_skip_the_host_defaults() {
    let mut v = vm(Media::Path("/isos/a b.iso".into()), None, Display::Vnc);
    (v.cpus, v.memory_gib) = (Some(4), Some(6));
    let s = script(&v);
    assert!(s.contains("cpus=4\nmem=6144\n"), "{s}");
    assert!(s.contains("ln -s \"$(realpath '/isos/a b.iso')\" \"$run/cdrom.iso\""), "{s}");
}

#[test]
fn staging_and_boot() {
    assert_eq!(stage("ab12"), "mkdir -m 700 /tmp/fspec-try-ab12 && cat > /tmp/fspec-try-ab12/try.sh");
    assert_eq!(
        remote_boot("g815", "ab12", 5901).to_string(),
        "ssh -tt -o ExitOnForwardFailure=yes -o ServerAliveInterval=15 -L 127.0.0.1:5901:/tmp/fspec-try-ab12/vnc.sock g815 'bash /tmp/fspec-try-ab12/try.sh'"
    );
    assert_eq!(vnc_socket("ab12"), "/tmp/fspec-try-ab12/vnc.sock");
}

#[test]
fn delete_checks_the_name() {
    assert!(delete("../x").is_err());
    assert!(delete("deb").unwrap().contains("rm -rf \"$data\"/deb\n"));
}

#[test]
fn viewers() {
    let mac = viewer("macos", 5901, "pw", |_| false).unwrap();
    assert_eq!(mac.to_string(), "open vnc://:pw@127.0.0.1:5901");
    let linux = viewer("linux", 5901, "pw", |p| p == "remote-viewer").unwrap();
    assert_eq!(linux.to_string(), "remote-viewer vnc://:pw@127.0.0.1:5901");
    assert!(viewer("linux", 5901, "pw", |_| false).is_none());
}

#[test]
fn unused_isos_expire() {
    let s = script(&vm(debian(), None, Display::Vnc));
    assert!(s.contains("touch \"$iso\"\nln -s \"$iso\" \"$run/cdrom.iso\""), "{s}");
    assert!(s.contains("find \"$cache/iso\" -type f -name '*.iso' -mmin +10080 -amin +10080"), "{s}");
    assert!(s.contains("readlink -f \"$d/cdrom.iso\""), "{s}");
    assert!(s.find("touch \"$iso\"") < s.find("-mmin +10080"), "{s}");
}

mod vm {
    use fspec_core::resource::Vm;

    use super::H;
    use crate::Iso;
    use crate::vm::*;

    fn decl() -> Vm {
        serde_json::from_value(serde_json::json!({
            "on": "g815", "port": 2222, "cpus": 6, "authorized_keys": ["ssh-ed25519 AAAA mac's key"],
        }))
        .unwrap()
    }

    fn guest() -> Guest {
        Guest {
            name: "fstestvm0".into(),
            user: "kyan".into(),
            uid: Some(1000),
            files: vec![("/etc/a.sh".into(), "echo a\n".into(), true)],
            first_boot: Some("bootctl install\n".into()),
        }
    }

    fn image(sha256: Option<&str>) -> Iso {
        let url = image_url("opensuse-tumbleweed", &decl()).unwrap();
        Iso { sha256: sha256.map(String::from), ..Iso::from_url(&url).unwrap() }
    }

    #[test]
    fn tumbleweed_has_a_default_image_and_others_need_one() {
        assert!(image(None).file.ends_with("Minimal-VM.x86_64-Cloud.qcow2"));
        let err = image_url("fedora", &decl()).unwrap_err().to_string();
        assert!(err.contains("set vm(image = ...)"), "{err}");
        let own = Vm { image: Some("https://x.org/a.qcow2".into()), ..decl() };
        assert_eq!(image_url("fedora", &own).unwrap(), "https://x.org/a.qcow2");
        let list = format!("{H}  openSUSE-Tumbleweed-Minimal-VM.x86_64-Cloud.qcow2\n");
        assert_eq!(image_sum(&list, &image(None)).as_deref(), Some(H));
    }

    #[test]
    fn the_seed_gives_the_first_user_its_keys_and_sudo() {
        let (data, meta) = seed(&guest(), &decl());
        assert!(data.starts_with("#cloud-config\n"));
        let v: serde_json::Value = serde_json::from_str(data.lines().nth(1).unwrap()).unwrap();
        assert_eq!(v["hostname"], "fstestvm0");
        assert_eq!(v["users"][0]["name"], "kyan");
        assert_eq!(v["users"][0]["uid"], 1000);
        assert_eq!(v["users"][0]["sudo"], "ALL=(ALL) NOPASSWD:ALL");
        assert_eq!(v["users"][0]["ssh_authorized_keys"][0], "ssh-ed25519 AAAA mac's key");
        let m: serde_json::Value = serde_json::from_str(&meta).unwrap();
        assert_eq!(m["instance-id"], "fstestvm0");
    }

    #[test]
    fn qemu_is_headless_and_forwards_ssh_on_the_loopback() {
        let line = qemu("fstestvm0", &decl(), false).to_string();
        for want in [
            "-smp 6",
            "-m 8G",
            "hostfwd=tcp:127.0.0.1:2222-:22",
            "-display none",
            "-serial file:serial.log",
            "-daemonize",
            "file=seed.img",
            "-device virtio-vga",
            "unix:rec.sock",
        ] {
            assert!(line.contains(want), "{want} in {line}");
        }
        assert!(!line.contains("-vnc"), "{line}");
        assert!(!line.contains(" -S"), "{line}");
        assert!(qemu("fstestvm0", &decl(), true).to_string().ends_with(" -S"));
    }

    #[test]
    fn the_seed_runs_the_first_boot_and_reports_on_the_serial_port() {
        let (data, _) = seed(&guest(), &decl());
        let v: serde_json::Value = serde_json::from_str(data.lines().nth(1).unwrap()).unwrap();
        assert_eq!(v["write_files"][0]["path"], "/etc/a.sh");
        assert_eq!(v["write_files"][0]["permissions"], "0755");
        assert_eq!(v["write_files"][1]["content"], "bootctl install\n");
        let run = v["runcmd"][0][2].as_str().unwrap();
        assert!(run.contains(&format!("then echo '{FIRST_BOOT_DONE}' >/dev/ttyS0; else")), "{run}");
        assert!(run.contains(FIRST_BOOT_FAILED), "{run}");
        let s = start(&guest(), &decl(), &image(Some(H)), None).unwrap();
        // Only a new disk waits for it: a later start's serial.log is truncated and never says it.
        assert!(s.contains("if [ -n \"$new\" ]; then"), "{s}");
        assert!(s.contains(&format!("grep -qF '{FIRST_BOOT_DONE}' serial.log")), "{s}");
    }

    #[test]
    fn a_boot_recording_takes_its_first_frame_before_the_firmware_runs() {
        let r = Record { every_ms: 100, seconds: 90 };
        let s = start(&guest(), &decl(), &image(Some(H)), Some(&r)).unwrap();
        assert!(s.contains("running && die"), "{s}");
        assert!(s.contains("-daemonize -S"), "{s}");
        let first = s.find("frames/000000.png").unwrap();
        assert!(first < s.find("\"execute\":\"cont\"").unwrap(), "{s}");
        assert!(s.contains("\"$abs\" 100 90"), "{s}");
        let live = record("fstestvm0", &r).unwrap();
        assert!(live.contains("running || die"), "{live}");
        assert!(!live.contains("cont"), "{live}");
        assert!(record("fstestvm0", &Record { every_ms: 0, seconds: 1 }).is_err());
        assert!(frames("fstestvm0").unwrap().contains("tar -C \"$dir\" -cf - frames"));
    }

    #[test]
    fn text_is_typed_as_us_keys() {
        let k = text_keys("aZ -_\n").unwrap();
        let want: Vec<Vec<&str>> =
            vec![vec!["a"], vec!["shift", "z"], vec!["spc"], vec!["minus"], vec!["shift", "minus"], vec!["ret"]];
        assert_eq!(k, want);
        assert_eq!(
            text_keys("\\|~").unwrap(),
            vec![vec!["backslash"], vec!["shift", "backslash"], vec!["shift", "grave_accent"]]
        );
        assert!(text_keys("ñ").unwrap_err().to_string().contains("fspec vm key"));
        assert_eq!(combo_keys("ctrl-alt-f2").unwrap(), vec!["ctrl", "alt", "f2"]);
        assert!(combo_keys("ctrl--x").is_err());
        let s = send_keys("fstestvm0", &[vec!["shift".into(), "a".into()]]).unwrap();
        assert!(s.contains(r#"{"type":"qcode","data":"shift"},{"type":"qcode","data":"a"}"#), "{s}");
    }

    #[test]
    fn start_creates_the_disk_once_and_waits_for_sshd() {
        let s = start(&guest(), &decl(), &image(Some(H)), None).unwrap();
        assert!(s.contains("if running; then"), "{s}");
        assert!(s.contains("if [ ! -f \"$dir/disk.qcow2\" ]; then"));
        assert!(s.contains(&format!("echo \"{H}  $image.part\" | sha256sum -c")));
        assert!(s.contains("qemu-img resize -q \"$dir/disk.qcow2.part\" 40G"));
        assert!(s.contains("mkfs.vfat -n CIDATA"));
        // The disk appears last, so a start that died half way creates it again.
        assert!(s.find("mv \"$dir/seed.img.part\"").unwrap() < s.find("mv \"$dir/disk.qcow2.part\"").unwrap());
        assert!(s.contains("dcache=none"), "{s}");
        assert!(s.contains("/dev/tcp/127.0.0.1/$port"));
        assert!(s.contains("ssh on 127.0.0.1:$port"));
    }

    #[test]
    fn start_refuses_to_download_an_image_it_cannot_check() {
        let s = start(&guest(), &decl(), &image(None), None).unwrap();
        assert!(s.contains("no published checksum for"), "{s}");
    }

    #[test]
    fn stop_powers_off_then_stops_qemu() {
        let s = stop("fstestvm0").unwrap();
        assert!(s.contains("system_powerdown"));
        assert!(s.find("system_powerdown").unwrap() < s.find("kill \"$pid\"").unwrap());
        assert!(stop("../x").is_err());
    }

    #[test]
    fn status_reports_each_vm() {
        let s = status(&[("fstestvm0", 2222), ("fstestvm1", 2223)]).unwrap();
        assert!(s.contains("fstestvm1 \"$state\" 2223"), "{s}");
        assert!(status(&[("a b", 1)]).is_err());
    }

    #[test]
    fn scripts_parse() {
        let scripts = [
            start(&guest(), &decl(), &image(Some(H)), None).unwrap(),
            stop("fstestvm0").unwrap(),
            status(&[("a", 1)]).unwrap(),
            start(&guest(), &decl(), &image(Some(H)), Some(&Record { every_ms: 100, seconds: 5 })).unwrap(),
            record("fstestvm0", &Record { every_ms: 100, seconds: 5 }).unwrap(),
            frames("fstestvm0").unwrap(),
            send_keys("fstestvm0", &text_keys("Hi!\n").unwrap()).unwrap(),
        ];
        for s in scripts {
            let out = std::process::Command::new("bash").args(["-n", "-c", &s]).output().unwrap();
            assert!(out.status.success(), "{}\n{s}", String::from_utf8_lossy(&out.stderr));
        }
    }
}
