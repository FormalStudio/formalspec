//! Secure Boot with the machine's own keys. sbctl runs from the live session to create and enrol
//! the keys, and inside the new root to sign what boots, so its database holds the installed
//! system's paths and the kernel hooks below keep signing after updates.

use fspec_core::runner::Cmd;

/// kernel-install plugin for Fedora, Debian and Ubuntu: signs the UKI the ukify plugin just wrote.
const KERNEL_INSTALL_PLUGIN: &str = r#"#!/bin/sh
uki="$KERNEL_INSTALL_BOOT_ROOT/EFI/Linux/$KERNEL_INSTALL_ENTRY_TOKEN-$2.efi"
case "$1" in
add) sbctl sign -s "$uki" >/dev/null ;;
remove) sbctl remove-file "$uki" >/dev/null || : ;;
esac
"#;

/// mkinitcpio post hook for Arch; `$3` is the UKI it wrote.
const MKINITCPIO_HOOK: &str = r#"#!/bin/sh
if [ -n "$3" ]; then sbctl sign -s "$3" >/dev/null; fi
"#;

/// Void's 60-fspec hook copies the kernel to the ESP unsigned on every kernel install.
const VOID_HOOK: &str = "#!/bin/sh\nsbctl sign -s /efi/void/vmlinuz >/dev/null\n";

/// `bootctl update` replaces systemd-boot with the unsigned copy from the package.
const BOOT_UPDATE_DROPIN: &str = "[Service]\nExecStartPost=-/bin/sh -c 'sbctl sign-all'\n";

const AERYNOS_SIGN: &str = "cd \"$1\" && find . -type f \\( -iname '*.efi' -o -name 'kernel*' -o -name 'vmlinuz*' \\) \
                            -not -path '*/Microsoft/*' -exec sbctl sign {} +";

/// What the install runs after the new root is built, in order: sign everything on the ESP, then
/// enrol the keys last, so a failed install leaves the firmware's keys alone.
///
/// `root` is the mounted new root, `efi` its ESP as mounted in the live session, and `loader` the
/// Windows ESP holding systemd-boot when installing beside Windows. AerynOS has no sbctl package
/// and moss writes its own boot files, so its kernels are signed once from the live session and a
/// kernel update leaves them unsigned.
pub fn steps(distro: &str, root: &str, efi: &str, loader: Option<&str>) -> Vec<Cmd> {
    let chroot = |args: &[&str]| {
        Cmd::new("chroot")
            .arg(root)
            .args(args.iter().copied())
            .env("HOME", "/root")
            .env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
            .env("DEBIAN_FRONTEND", "noninteractive")
    };
    let write = |path: &str, text: &str, exec: bool| {
        let dir = path.rsplit_once('/').map_or("", |(d, _)| d);
        let mut cmds = vec![
            Cmd::new("mkdir").arg("-p").arg(format!("{root}{dir}")),
            Cmd::new("tee").arg(format!("{root}{path}")).stdin(text),
        ];
        if exec {
            cmds.push(Cmd::new("chmod").arg("755").arg(format!("{root}{path}")));
        }
        cmds
    };
    let mut cmds = vec![Cmd::new("sbctl").arg("create-keys")];

    if distro == "aerynos" {
        cmds.push(Cmd::new("sh").args(["-c", AERYNOS_SIGN, "sh", efi]));
    } else {
        cmds.extend(match distro {
            "arch" => vec![chroot(&["pacman", "-S", "--noconfirm", "--needed", "sbctl"])],
            "fedora" => vec![chroot(&["dnf", "-y", "install", "sbctl"])],
            "ubuntu" => vec![chroot(&["apt-get", "install", "-y", "sbctl"])],
            "void" => vec![chroot(&["xbps-install", "-Sy", "sbctl"])],
            // Debian has no sbctl package; the live session's build links Debian's own libc.
            _ => vec![
                Cmd::new("install")
                    .args(["-D", "-m", "0755", "/usr/local/bin/sbctl"])
                    .arg(format!("{root}/usr/local/bin/sbctl")),
                chroot(&["apt-get", "install", "-y", "libpcsclite1"]),
            ],
        });
        cmds.push(Cmd::new("mkdir").arg("-p").arg(format!("{root}/var/lib")));
        cmds.push(Cmd::new("cp").arg("-a").arg("/var/lib/sbctl").arg(format!("{root}/var/lib/sbctl")));

        match distro {
            "arch" => cmds.extend(write("/etc/initcpio/post/sbctl", MKINITCPIO_HOOK, true)),
            "void" => cmds.extend(write("/etc/kernel.d/post-install/70-fspec-sbctl", VOID_HOOK, true)),
            _ => cmds.extend(write("/etc/kernel/install.d/91-sbctl.install", KERNEL_INSTALL_PLUGIN, true)),
        }
        if distro != "void" {
            cmds.extend(write(
                "/etc/systemd/system/systemd-boot-update.service.d/sbctl.conf",
                BOOT_UPDATE_DROPIN,
                false,
            ));
        }

        let mut sign = String::from("set -e\n");
        if loader.is_none() {
            sign += "sbctl sign -s /efi/EFI/systemd/systemd-bootx64.efi\nsbctl sign -s /efi/EFI/BOOT/BOOTX64.EFI\n";
        }
        if distro == "void" {
            sign += "sbctl sign -s /efi/void/vmlinuz\n";
        } else {
            sign += "for f in /efi/EFI/Linux/*.efi; do if [ -e \"$f\" ]; then sbctl sign -s \"$f\"; fi; done\n";
        }
        cmds.push(chroot(&["sh", "-c", &sign]));
    }
    // Beside Windows, systemd-boot sits on Windows' ESP, outside the new root.
    if let Some(loader) = loader {
        cmds.push(Cmd::new("sbctl").arg("sign").arg(format!("{loader}/EFI/systemd/systemd-bootx64.efi")));
    }
    cmds.push(Cmd::new("sbctl").args(["enroll-keys", "--microsoft"]));
    cmds
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(distro: &str, loader: Option<&str>) -> Vec<String> {
        steps(distro, "/r/arch", "/r/efi", loader).iter().map(ToString::to_string).collect()
    }

    #[test]
    fn every_systemd_distro_gets_a_hook_that_signs_its_next_kernel() {
        for (distro, hook) in [
            ("arch", "/r/arch/etc/initcpio/post/sbctl"),
            ("fedora", "/r/arch/etc/kernel/install.d/91-sbctl.install"),
            ("debian", "/r/arch/etc/kernel/install.d/91-sbctl.install"),
            ("ubuntu", "/r/arch/etc/kernel/install.d/91-sbctl.install"),
            ("void", "/r/arch/etc/kernel.d/post-install/70-fspec-sbctl"),
        ] {
            let l = lines(distro, None);
            assert!(l.iter().any(|x| x.starts_with(&format!("tee {hook}"))), "{distro}: {l:#?}");
            assert_eq!(l.first().map(String::as_str), Some("sbctl create-keys"));
            assert_eq!(l.last().map(String::as_str), Some("sbctl enroll-keys --microsoft"));
        }
        assert!(lines("void", None).iter().any(|l| l.contains("sbctl sign -s /efi/void/vmlinuz")));
        assert!(lines("debian", None).iter().any(|l| l.starts_with("install -D -m 0755 /usr/local/bin/sbctl")));
    }

    #[test]
    fn aerynos_is_signed_once_from_the_live_session() {
        let l = lines("aerynos", None);
        assert!(l.iter().all(|x| !x.starts_with("chroot")), "{l:#?}");
        assert!(l.iter().any(|x| x.contains("sbctl sign {} +") && x.ends_with("/r/efi")));
    }

    #[test]
    fn beside_windows_the_boot_manager_on_its_esp_is_signed_from_the_live_session() {
        let l = lines("arch", Some("/r/esp"));
        assert!(l.contains(&"sbctl sign /r/esp/EFI/systemd/systemd-bootx64.efi".to_string()));
        assert!(!l.iter().any(|x| x.contains("systemd-bootx64.efi") && x.starts_with("chroot")));
    }
}
