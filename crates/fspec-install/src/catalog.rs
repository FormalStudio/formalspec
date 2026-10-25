//! The distros the installer offers, in the order the picker lists them.

use crate::model::Delivery::{Atomic, Classic};
use crate::model::Desktop::{Cosmic, Gnome, Kde};
use crate::model::Init::{Runit, Systemd};
use crate::model::Preset::{Desktop, Gaming, Minimal, Server};
use crate::model::Release::{Fixed, Rolling};
use crate::model::{Distro, Preset};

const ALL: &[Preset] = &[Minimal, Desktop, Gaming, Server];

pub const DISTROS: [Distro; 6] = [
    Distro {
        id: "fedora",
        name: "Fedora",
        reason: "Current software every six months. Secure Boot ready.",
        release: Fixed,
        init: Systemd,
        signed_kernel: true,
        delivery: Classic,
        based_on: None,
        prerelease: None,
        snapper: false,
        needs_efi: false,
        presets: ALL,
        desktops: &[Gnome, Kde],
    },
    Distro {
        id: "arch",
        name: "Arch Linux",
        reason: "Newest packages, and only what you ask for.",
        release: Rolling,
        init: Systemd,
        signed_kernel: false,
        delivery: Classic,
        based_on: None,
        prerelease: None,
        snapper: false,
        needs_efi: false,
        presets: ALL,
        desktops: &[Kde, Gnome],
    },
    Distro {
        id: "ubuntu",
        name: "Ubuntu",
        reason: "Long-term support with broad hardware support.",
        release: Fixed,
        init: Systemd,
        signed_kernel: true,
        delivery: Classic,
        based_on: Some("Debian"),
        prerelease: None,
        snapper: false,
        needs_efi: false,
        presets: ALL,
        desktops: &[Gnome, Kde],
    },
    Distro {
        id: "debian",
        name: "Debian",
        reason: "Stable for years. Nothing changes until you upgrade.",
        release: Fixed,
        init: Systemd,
        signed_kernel: true,
        delivery: Classic,
        based_on: None,
        prerelease: None,
        snapper: false,
        needs_efi: false,
        presets: ALL,
        desktops: &[Gnome, Kde],
    },
    Distro {
        id: "void",
        name: "Void Linux",
        reason: "Independent rolling distro with runit.",
        release: Rolling,
        init: Runit,
        signed_kernel: false,
        delivery: Classic,
        based_on: None,
        prerelease: None,
        snapper: false,
        needs_efi: false,
        presets: ALL,
        desktops: &[Kde, Gnome],
    },
    Distro {
        id: "aerynos",
        name: "AerynOS",
        reason: "Built from scratch, with atomic updates and rollback.",
        release: Rolling,
        init: Systemd,
        signed_kernel: false,
        delivery: Atomic,
        based_on: None,
        prerelease: Some("alpha"),
        snapper: false,
        needs_efi: true,
        presets: &[Desktop],
        desktops: &[Gnome, Kde, Cosmic],
    },
];

pub fn distro(id: &str) -> Option<&'static Distro> {
    DISTROS.iter().find(|d| d.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_every_distro_has_a_preset() {
        for (i, d) in DISTROS.iter().enumerate() {
            assert!(DISTROS[..i].iter().all(|o| o.id != d.id), "{} listed twice", d.id);
            assert!(!d.presets.is_empty(), "{} has no preset", d.id);
            let needs_desktop = d.presets.iter().any(|p| matches!(p, Desktop | Gaming));
            assert_eq!(needs_desktop, !d.desktops.is_empty(), "{} desktops", d.id);
        }
    }
}
