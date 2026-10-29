use std::fmt;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::resource::System;

/// What a spec knows about a host, derived from its `host()` declaration so evaluation stays pure.
/// Module functions receive this. `apply` probes the machine only to check it matches.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Facts {
    pub name: String,
    pub os: Os,
    /// `x86_64` or `aarch64`.
    pub arch: String,
    /// `fedora`, `bazzite`, `arch`, `void`, `macos`, …
    pub distro: String,
    pub init: Init,
    /// The system layer is an `image(...)`: distro packages belong to the image, not to a live package manager.
    #[serde(default)]
    pub image: bool,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Os {
    Linux,
    Darwin,
}

impl fmt::Display for Os {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            Os::Linux => "linux",
            Os::Darwin => "darwin",
        })
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Init {
    Systemd,
    Runit,
    Openrc,
    Dinit,
    S6,
    Launchd,
}

impl fmt::Display for Init {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let s = serde_json::to_value(self).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default();
        f.write_str(&s)
    }
}

impl Facts {
    /// `platform` is `<arch>-<os>`, e.g. `x86_64-linux` or `aarch64-darwin`.
    pub fn declared(name: &str, platform: &str, system: Option<&System>) -> Result<Facts> {
        let Some((arch, os)) = platform.split_once('-') else {
            bail!("host {name}: platform `{platform}` is not <arch>-<os>, e.g. x86_64-linux")
        };
        if !matches!(arch, "x86_64" | "aarch64") {
            bail!("host {name}: unknown arch `{arch}` in platform `{platform}`");
        }
        let os = match os {
            "linux" => Os::Linux,
            "darwin" => Os::Darwin,
            _ => bail!("host {name}: unknown os `{os}` in platform `{platform}`"),
        };
        let (distro, init) = match (os, system) {
            (Os::Darwin, None) => ("macos".into(), Init::Launchd),
            (Os::Darwin, Some(_)) => bail!("host {name}: macOS hosts take no system layer"),
            (Os::Linux, Some(System::Image(i))) => (i.distro(), Init::Systemd),
            (Os::Linux, Some(System::Distro(d))) => (d.distro.clone(), d.init),
            (Os::Linux, None) => bail!("host {name}: a Linux host needs system = image(...) or a distro"),
        };
        let image = matches!(system, Some(System::Image(_)));
        Ok(Facts { name: name.into(), os, arch: arch.into(), distro, init, image })
    }

    /// Platform as the lock writes it, e.g. `aarch64-darwin`.
    pub fn platform(&self) -> String {
        format!("{}-{}", self.arch, self.os)
    }

    /// The machine this process runs on, for checking a declaration against reality.
    pub fn probe_local() -> (String, String) {
        let os = if cfg!(target_os = "macos") { "darwin" } else { "linux" };
        let distro = match os {
            "darwin" => "macos".into(),
            _ => std::fs::read_to_string("/etc/os-release")
                .ok()
                .and_then(|s| os_release_id(&s))
                .unwrap_or_else(|| "linux".into()),
        };
        (format!("{}-{os}", std::env::consts::ARCH), distro)
    }
}

pub fn os_release_id(contents: &str) -> Option<String> {
    contents.lines().find_map(|l| l.strip_prefix("ID=")).map(|v| v.trim_matches('"').to_string())
}

/// Names this machine's host when its OS host name is not the one the spec uses (a Mac called
/// `MacBook-Pro-2` whose tailnet name every alias depends on). The spec writes it, so after one
/// apply the machine knows itself.
pub const HOST_FILE: &str = "/etc/fspec/host";

/// Which `host(name = …)` this machine is: `$FSPEC_HOST`, else [`HOST_FILE`], else its short host
/// name.
pub fn local_hostname() -> String {
    let declared = std::env::var("FSPEC_HOST").ok().or_else(|| std::fs::read_to_string(HOST_FILE).ok());
    match declared.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        Some(name) => name.to_lowercase(),
        None => os_hostname(),
    }
}

/// This machine's short host name, as `host(name = …)` usually spells it.
pub fn os_hostname() -> String {
    // Void's `hostname` has no `-s`, and minimal Arch and Fedora installs ship no `hostname` at all.
    #[cfg(target_os = "linux")]
    let name = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
    #[cfg(not(target_os = "linux"))]
    let name = std::process::Command::new("hostname")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    name.trim().split('.').next().unwrap_or_default().to_lowercase()
}
