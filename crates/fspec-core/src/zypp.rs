//! The Tumbleweed snapshot repos a zypper host is pinned to, written both by `apply` and by the
//! bootstrap that installs Tumbleweed, so a fresh root starts without repo drift.

pub const HISTORY: &str = "https://download.opensuse.org/history";
/// The zypp repo variable (`/etc/zypp/vars.d/<name>`) the snapshot repos' URLs expand, so
/// moving the pin rewrites one file and leaves the repo definitions alone.
pub const SNAPSHOT_VAR: &str = "fspec_snapshot";
/// Whether Tumbleweed on `arch` has history snapshots to pin: openSUSE keeps them for x86_64,
/// not for the ports.
pub fn pinned(arch: &str) -> bool {
    arch == "x86_64"
}

/// The snapshot repos: alias and path under `<HISTORY>/<snapshot>/tumbleweed/`.
pub const SNAPSHOT_REPOS: [(&str, &str); 2] = [("fspec-oss", "repo/oss/"), ("fspec-non-oss", "repo/non-oss/")];

/// The `/etc/zypp/repos.d/<alias>.repo` file of a snapshot repo.
pub fn snapshot_repo(alias: &str, path: &str) -> String {
    format!(
        "[{alias}]\nname=openSUSE Tumbleweed {path} at the fspec.lock snapshot\nenabled=1\nautorefresh=1\n\
         baseurl={HISTORY}/${{{SNAPSHOT_VAR}}}/tumbleweed/{path}\ntype=rpm-md\ngpgcheck=1\n"
    )
}
