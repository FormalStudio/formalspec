use std::path::Path;

use fspec_core::resource::Source;
use fspec_core::runner::Cmd;

/// The distro package managers FormalSpec drives. Everything that differs between them is
/// a command line, so the install, cache and journal logic is shared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Family {
    Dnf,
    Pacman,
    Apt,
    Xbps,
    Moss,
    Zypper,
}

impl Family {
    pub fn of_distro(distro: &str) -> Option<Family> {
        match distro {
            "fedora" => Some(Family::Dnf),
            "arch" => Some(Family::Pacman),
            "debian" | "ubuntu" => Some(Family::Apt),
            "void" => Some(Family::Xbps),
            "aerynos" => Some(Family::Moss),
            "opensuse-tumbleweed" => Some(Family::Zypper),
            _ => None,
        }
    }

    pub fn of_source(source: &Source) -> Option<Family> {
        match source {
            Source::Dnf(_) => Some(Family::Dnf),
            Source::Pacman(_) => Some(Family::Pacman),
            Source::Apt(_) => Some(Family::Apt),
            Source::Xbps(_) => Some(Family::Xbps),
            Source::Moss(_) => Some(Family::Moss),
            Source::Zypper(_) => Some(Family::Zypper),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Family::Dnf => "dnf",
            Family::Pacman => "pacman",
            Family::Apt => "apt",
            Family::Xbps => "xbps",
            Family::Moss => "moss",
            Family::Zypper => "zypper",
        }
    }

    pub fn install_names(self, names: &[String]) -> Vec<Cmd> {
        vec![match self {
            Family::Dnf => dnf_install().args(names),
            Family::Pacman => Cmd::new("pacman").args(["-S", "--noconfirm", "--needed"]).args(names),
            Family::Apt => apt("install").args(names),
            Family::Xbps => Cmd::new("xbps-install").arg("-y").args(names),
            Family::Moss => moss(["-y", "install"].into_iter().map(String::from).chain(names.iter().cloned())),
            Family::Zypper => zypper_install().args(names),
        }]
    }

    /// Install a cached package file. A local xbps repository needs its index built first, and
    /// installs by name from it. moss packages lock by name only, so they never have a file.
    pub fn install_file(self, file: &Path, names: &[String]) -> Vec<Cmd> {
        let file = file.display().to_string();
        match self {
            Family::Dnf => vec![dnf_install().arg(file)],
            Family::Pacman => vec![Cmd::new("pacman").args(["-U", "--noconfirm", "--needed"]).arg(file)],
            Family::Apt => vec![apt("install").arg(file)],
            Family::Xbps => {
                let dir = Path::new(&file).parent().map(|d| d.display().to_string()).unwrap_or_default();
                vec![
                    Cmd::new("xbps-rindex").arg("-a").arg(&file),
                    Cmd::new("xbps-install").args(["-y", "-R"]).arg(dir).args(names),
                ]
            }
            Family::Moss => self.install_names(names),
            Family::Zypper => vec![zypper_install().arg(file)],
        }
    }

    pub fn remove(self, names: &[String]) -> Cmd {
        match self {
            Family::Dnf => Cmd::new("dnf").args(["remove", "-y"]).args(names),
            Family::Pacman => Cmd::new("pacman").args(["-R", "--noconfirm"]).args(names),
            Family::Apt => apt("remove").args(names),
            Family::Xbps => Cmd::new("xbps-remove").arg("-y").args(names),
            Family::Moss => moss(["-y", "remove"].into_iter().map(String::from).chain(names.iter().cloned())),
            // A dropped name takes the packages only it pulled in, as rust-src does the distro
            // toolchain, which would otherwise stay and conflict with what replaces it.
            Family::Zypper => zypper(["remove", "--clean-deps"]).args(names),
        }
    }

    pub fn version(self, name: &str) -> Cmd {
        match self {
            // A package provides its own name, so this also answers a capability (`pkgconfig(pam)`,
            // a file path), which plain `rpm -q` reports as not installed.
            Family::Dnf | Family::Zypper => {
                Cmd::new("rpm").args(["-q", "--whatprovides", "--qf", r"%{VERSION}-%{RELEASE}\n"]).arg(name)
            }
            Family::Pacman => Cmd::new("pacman").arg("-Q").arg(name),
            Family::Apt => Cmd::new("dpkg-query").args(["-W", "-f=${Version}"]).arg(name),
            Family::Xbps => Cmd::new("xbps-query").args(["-p", "pkgver"]).arg(name),
            // `moss list installed` prints `name  version-release - summary`, and has no query for
            // one package. Plain sh, since AerynOS's base has no awk or grep.
            Family::Moss => Cmd::new("sh").args([
                "-c",
                r#"moss list installed | { while read -r n v _; do if [ "$n" = "$1" ]; then echo "$v"; exit 0; fi; done; exit 1; }"#,
                "sh",
                name,
            ]),
        }
    }

    /// One query for the installed versions of many names, printing `name<TAB>version` per
    /// installed package, for the managers that have one. Names it misses (a capability, a
    /// `name:arch`) go through [`Family::version`] one at a time.
    pub fn versions(self, names: &[String]) -> Option<Cmd> {
        match self {
            Family::Dnf | Family::Zypper => {
                Some(Cmd::new("rpm").args(["-q", "--qf", r"%{NAME}\t%{VERSION}-%{RELEASE}\n"]).args(names))
            }
            Family::Pacman => Some(Cmd::new("pacman").arg("-Q").args(names)),
            Family::Apt => Some(Cmd::new("dpkg-query").args(["-W", r"-f=${Package}\t${Version}\n"]).args(names)),
            Family::Xbps | Family::Moss => None,
        }
    }

    /// `(name, version)` per installed package out of a [`Family::versions`] query. Lines that
    /// are not a package, such as rpm's `package x is not installed`, are skipped.
    pub fn parse_versions(self, stdout: &str) -> Vec<(String, String)> {
        stdout
            .lines()
            .filter_map(|l| match self {
                Family::Pacman => l.split_once(' '),
                _ => l.split_once('\t'),
            })
            .map(|(n, v)| (n.trim().to_string(), v.trim().to_string()))
            .filter(|(n, v)| !n.is_empty() && !v.is_empty())
            .collect()
    }

    /// The installed version out of a successful [`Family::version`] query.
    pub fn parse_version(self, name: &str, stdout: &str) -> String {
        let out = stdout.trim();
        match self {
            Family::Pacman => out.split_whitespace().nth(1).unwrap_or_default().to_string(),
            Family::Xbps => out.strip_prefix(&format!("{name}-")).unwrap_or(out).to_string(),
            // Several packages can provide one capability; the first answers.
            Family::Dnf | Family::Zypper => out.lines().next().unwrap_or_default().to_string(),
            Family::Apt | Family::Moss => out.to_string(),
        }
    }
}

/// Where moss's boot sync rewrites the default entry on the shared layout.
pub(crate) const LOADER_CONF: &str = "/efi/loader/loader.conf";

fn moss(args: impl IntoIterator<Item = String>) -> Cmd {
    fspec_core::moss::moss(LOADER_CONF, args)
}

impl Family {
    /// The command that puts the packages back as they were before the next change, when the
    /// manager keeps whole-system states: moss records one per transaction in `/usr/.stateID`.
    pub fn restore_point(self, root: &Path) -> Option<Cmd> {
        if self != Family::Moss {
            return None;
        }
        let id = std::fs::read_to_string(root.join("usr/.stateID")).ok()?;
        let id: u64 = id.trim().parse().ok()?;
        Some(moss(["-y".into(), "state".into(), "activate".into(), id.to_string()]))
    }
}

/// zypper with `--non-interactive`, a global option that has to come before the command.
pub(crate) fn zypper<'a>(args: impl IntoIterator<Item = &'a str>) -> Cmd {
    Cmd::new("zypper").arg("--non-interactive").args(args)
}

fn zypper_install() -> Cmd {
    zypper(["install", "--auto-agree-with-licenses"])
}

/// dnf fetches three packages at a time by default, which leaves a desktop's first install
/// waiting on one round trip per package.
fn dnf_install() -> Cmd {
    Cmd::new("dnf").args(["install", "-y", "--setopt=max_parallel_downloads=10"])
}

fn apt(verb: &str) -> Cmd {
    Cmd::new("apt-get").args([verb, "-y"]).env("DEBIAN_FRONTEND", "noninteractive")
}
