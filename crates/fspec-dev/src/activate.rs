//! A project's environment: its locked packages on PATH through a directory of links, its
//! system dependencies checked, `env` set and `shell_hook` run. Nothing is installed globally.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use fspec_core::dev::Dev;
use fspec_core::facts::{Facts, Init, Os};
use fspec_core::fetch::Fetch;
use fspec_core::journal::remove_any;
use fspec_core::runner::Runner;
use fspec_core::{Cx, Lock, Paths, Scope};

use crate::container::{self, Run};
use crate::{Project, cache_dir, libs, python, short_hash, user_store};

/// The machine a project is activated on.
pub struct Machine<'a> {
    pub paths: &'a Paths,
    pub runner: &'a dyn Runner,
    pub fetch: &'a dyn Fetch,
    /// `aarch64-darwin`.
    pub platform: String,
    /// The package manager `lib(...)` and `bin(...)` are checked against; none on NixOS.
    pub manager: Option<&'static str>,
    /// The calling environment.
    pub base: BTreeMap<String, String>,
    /// `docker` or `podman`, when one is installed.
    pub runtime: Option<String>,
}

pub struct Activated {
    pub project: Project,
    pub dev: Dev,
    pub lock: Lock,
    /// The whole environment of the shell, or of the shell inside the container.
    pub env: BTreeMap<String, String>,
    /// Set when `dev.star` names a container.
    pub run: Option<Run>,
}

pub fn activate(project: &Project, m: &Machine) -> Result<Activated> {
    let lock_path = project.lock_path();
    if !lock_path.is_file() {
        bail!("{} has no fspec.lock beside it, run fspec update there", project.file().display());
    }
    let lock = Lock::load(&lock_path)?;
    let here = project.evaluate(&m.platform)?;
    if !here.platforms.contains(&m.platform) {
        bail!("{} does not support {} (platforms = {:?})", project.file().display(), m.platform, here.platforms);
    }
    let image = match (&here.container, &here.fallback_container) {
        (Some(image), _) => Some(image.clone()),
        (None, Some(fallback)) => {
            let misses = native_misses(&here, &lock, m.manager);
            if misses.is_empty() {
                None
            } else {
                eprintln!("fspec: {}; running in {fallback} (fallback_container)", misses.join("; "));
                Some(fallback.clone())
            }
        }
        (None, None) => None,
    };
    if let Some(image) = image {
        let arch = m.platform.split('-').next().unwrap_or("x86_64");
        let platform = format!("{arch}-linux");
        let dev = if platform == m.platform { here } else { project.evaluate(&platform)? };
        let farm = link_farm(project, &dev, &lock, m, &platform)?;
        let (run, env) = container::prepare(project, &dev, &image, &lock, &farm, m)?;
        return Ok(Activated { project: project.clone(), dev, lock, env, run: Some(run) });
    }
    let dev = here;
    let farm = link_farm(project, &dev, &lock, m, &m.platform)?;
    let mut env = m.base.clone();
    let mut path = vec![farm.dir.join("bin").display().to_string()];
    let mut search: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for prefix in check_natives(&dev, &lock, m)? {
        path.push(prefix.join("bin").display().to_string());
        let pc = [prefix.join("lib/pkgconfig"), prefix.join("share/pkgconfig")];
        search.entry("PKG_CONFIG_PATH").or_default().extend(pc.iter().map(|p| p.display().to_string()));
        search.entry("LIBRARY_PATH").or_default().push(prefix.join("lib").display().to_string());
        search.entry("CPATH").or_default().push(prefix.join("include").display().to_string());
    }
    if let Some(old) = m.base.get("PATH") {
        path.push(old.clone());
    }
    env.insert("PATH".into(), path.join(":"));
    for (k, dirs) in search {
        let mut v = dirs.join(":");
        if let Some(old) = m.base.get(k).filter(|o| !o.is_empty()) {
            v = format!("{v}:{old}");
        }
        env.insert(k.into(), v);
    }
    env.extend(farm.pkg_env());
    env.extend(dev.env.clone());
    env.insert("FSPEC_DEV".into(), project.dir.display().to_string());
    if let Some(hook) = &dev.shell_hook {
        env = run_hook(hook, &env, &project.dir)?;
    }
    Ok(Activated { project: project.clone(), dev, lock, env, run: None })
}

/// Why this machine's package manager cannot provide some `lib(...)` or `bin(...)`: misses the
/// lock recorded for it, or no package manager at all. Entries simply not locked yet are left to
/// `check_natives`, which asks for `fspec update`.
pub fn native_misses(dev: &Dev, lock: &Lock, manager: Option<&str>) -> Vec<String> {
    if libs::natives(dev).is_empty() {
        return vec![];
    }
    let Some(manager) = manager else { return vec!["this system has no package manager fspec knows".into()] };
    let mut missed = Vec::new();
    for (kind, list) in [("lib", &dev.libs), ("bin", &dev.bins)] {
        for n in list {
            let locked = lock.native(kind, &n.name);
            if locked.is_some_and(|l| !l.provides.contains_key(manager) && l.miss.contains_key(manager)) {
                missed.push(format!("{kind} {}", n.name));
            }
        }
    }
    if missed.is_empty() { vec![] } else { vec![format!("{manager} has no package for {}", missed.join(", "))] }
}

/// Check every `lib(...)` and `bin(...)` is installed, and return the Homebrew prefixes of the
/// ones brew provides, which are not on the default search paths.
fn check_natives(dev: &Dev, lock: &Lock, m: &Machine) -> Result<Vec<PathBuf>> {
    let natives = libs::natives(dev);
    if natives.is_empty() {
        return Ok(vec![]);
    }
    let Some(manager) = m.manager else {
        bail!(
            "this system has no package manager fspec checks lib(...) and bin(...) against; set fallback_container = \"debian:13\" in dev.star to get them from an image"
        );
    };
    let (mut problems, mut missing, mut prefixes) = (Vec::new(), Vec::new(), Vec::new());
    let brew = libs::brew_prefix();
    for ((is_lib, name), (_, answer)) in natives.iter().zip(libs::locked_for(dev, lock, manager)) {
        match answer {
            Err(why) => problems.push(why),
            Ok(pkg) if manager == "brew" => {
                let prefix = brew.join("opt").join(&pkg);
                if prefix.is_dir() {
                    prefixes.push(prefix);
                } else {
                    missing.push(pkg);
                }
            }
            Ok(pkg) => {
                if !libs::present_on_linux(Path::new("/"), *is_lib, name) {
                    missing.push(pkg);
                }
            }
        }
    }
    prefixes.dedup();
    missing.dedup();
    if !missing.is_empty() {
        problems.push(format!("not installed, install with: {}", libs::install_hint(manager, &missing)));
    }
    if !problems.is_empty() {
        bail!("dev.star needs system packages:\n  {}", problems.join("\n  "));
    }
    Ok(prefixes)
}

/// Run `f` with a store context for `platform`: the system store to reuse and the user's own.
pub(crate) fn with_store<R>(
    paths: &Paths,
    runner: &dyn Runner,
    fetch: &dyn Fetch,
    project: &Project,
    lock: &Lock,
    platform: &str,
    f: impl FnOnce(&Cx, &Path, &Path) -> Result<R>,
) -> Result<R> {
    let (arch, os) = platform.split_once('-').context("platform is <arch>-<os>")?;
    let facts = Facts {
        name: "dev".into(),
        os: if os == "darwin" { Os::Darwin } else { Os::Linux },
        arch: arch.into(),
        distro: "dev".into(),
        init: Init::Systemd,
        image: false,
    };
    let cx = Cx {
        paths,
        facts: &facts,
        spec_dir: &project.dir,
        lock,
        runner,
        fetch,
        generation: 0,
        scope: Scope::User,
        protect: &[],
        builders: &[],
        build_cache: None,
        checkout: None,
    };
    f(&cx, &paths.fspec_root().join("store"), &user_store(&paths.home))
}

/// The project's packages: a directory of links to their commands, and each package's tree.
pub(crate) struct Farm {
    pub dir: PathBuf,
    pub pkgs: BTreeMap<String, PathBuf>,
}

impl Farm {
    /// `FSPEC_PKG_YT_DLP=<tree>` for each package, so `env` and `shell_hook` can point into one.
    pub fn pkg_env(&self) -> BTreeMap<String, String> {
        self.pkgs.iter().map(|(name, dir)| (pkg_var(name), dir.display().to_string())).collect()
    }
}

pub(crate) fn pkg_var(name: &str) -> String {
    let upper: String =
        name.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' }).collect();
    format!("FSPEC_PKG_{upper}")
}

/// Unpack every locked package for `platform` and link their commands into a directory named by
/// what it holds, so it is built once and shared by every shell of the project.
pub(crate) fn link_farm(project: &Project, dev: &Dev, lock: &Lock, m: &Machine, platform: &str) -> Result<Farm> {
    let pypi = dev.packages.iter().any(python::is_pypi);
    if pypi && platform != m.platform {
        bail!("pypi(...) packages are installed for this machine and cannot run in a container yet");
    }
    let unpacked = with_store(m.paths, m.runner, m.fetch, project, lock, platform, |cx, shared, own| {
        let mut out: Vec<(String, fspec_store::Unpacked)> = Vec::new();
        for p in dev.packages.iter().filter(|p| !python::is_pypi(p)) {
            out.push((p.name.clone(), fspec_store::unpack_for_project(cx, p, shared, own)?));
        }
        if let Some((python_pkg, uv_pkg)) = python::tools(dev)? {
            let dir = |name: &str| out.iter().find(|(n, _)| n == name).map(|(_, u)| u.clone());
            let py = dir(&python_pkg.name).context("python is unpacked first")?;
            let uv = dir(&uv_pkg.name).context("uv is unpacked first")?;
            let uv_bin = uv.dir.join(uv.bins.get("uv").context("the uv package has no uv command")?);
            for p in dev.packages.iter().filter(|p| python::is_pypi(p)) {
                let locked = lock
                    .pkg(&p.name, &p.source.to_string())
                    .with_context(|| format!("pkg {} is not in fspec.lock, run fspec update", p.name))?;
                out.push((p.name.clone(), python::venv(m.runner, &uv_bin, &py.dir, locked, own)?));
            }
        }
        Ok(out)
    })?;
    let mut links: BTreeMap<String, (String, PathBuf)> = BTreeMap::new();
    let mut pkgs = BTreeMap::new();
    for (name, u) in unpacked {
        for (cmd, rel) in &u.bins {
            if let Some((other, _)) = links.get(cmd) {
                bail!("pkg {other} and pkg {name} both provide `{cmd}`");
            }
            links.insert(cmd.clone(), (name.clone(), u.dir.join(rel)));
        }
        pkgs.insert(name, u.dir);
    }
    let identity: Vec<String> = links.iter().map(|(c, (_, t))| format!("{c}={}", t.display())).collect();
    let key = short_hash(&format!("{}\n{platform}\n{}", project.dir.display(), identity.join("\n")));
    let farm = cache_dir(&m.paths.home).join("farms").join(&key);
    if !farm.is_dir() {
        let tmp = farm.with_extension(format!("tmp{}", std::process::id()));
        if tmp.exists() {
            remove_any(&tmp)?;
        }
        fs::create_dir_all(tmp.join("bin"))?;
        for (cmd, (_, target)) in &links {
            symlink(target, tmp.join("bin").join(cmd))?;
        }
        fs::write(tmp.join("project"), project.dir.display().to_string())?;
        if fs::rename(&tmp, &farm).is_err() {
            remove_any(&tmp)?;
        }
    }
    // gc keeps farms that were used recently, and the store trees they name.
    fs::write(farm.join("project"), project.dir.display().to_string())?;
    let trees: Vec<String> = pkgs.values().map(|d| d.display().to_string()).collect();
    fs::write(farm.join("trees"), trees.join("\n"))?;
    Ok(Farm { dir: farm, pkgs })
}

/// Run `hook` in sh with `env` and return the environment it leaves.
fn run_hook(hook: &str, env: &BTreeMap<String, String>, dir: &Path) -> Result<BTreeMap<String, String>> {
    let script = format!("{{\n{hook}\n}} >&2 && exec env -0");
    let out = std::process::Command::new("sh")
        .args(["-c", &script])
        .env_clear()
        .envs(env)
        .current_dir(dir)
        .stderr(std::process::Stdio::inherit())
        .output()
        .context("running shell_hook")?;
    if !out.status.success() {
        bail!("shell_hook in dev.star failed ({})", out.status);
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text.split('\0').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect())
}

/// Shell variables a subshell changes on its own, which an export must leave alone.
const VOLATILE: [&str; 4] = ["_", "SHLVL", "PWD", "OLDPWD"];

/// bash code that turns `base` into `env`, for direnv.
pub fn export(base: &BTreeMap<String, String>, env: &BTreeMap<String, String>) -> String {
    let quote = |v: &str| format!("'{}'", v.replace('\'', r"'\''"));
    let mut out = String::new();
    for (k, v) in env {
        if !VOLATILE.contains(&k.as_str()) && base.get(k) != Some(v) {
            out.push_str(&format!("export {k}={}\n", quote(v)));
        }
    }
    for k in base.keys() {
        if !VOLATILE.contains(&k.as_str()) && !env.contains_key(k) {
            out.push_str(&format!("unset {k}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_sets_changes_and_unsets_removals() {
        let base: BTreeMap<String, String> =
            [("PATH", "/usr/bin"), ("RUSTC", "x"), ("SHLVL", "1")].map(|(k, v)| (k.into(), v.into())).into();
        let env: BTreeMap<String, String> =
            [("PATH", "/farm/bin:/usr/bin"), ("A", "it's"), ("SHLVL", "2")].map(|(k, v)| (k.into(), v.into())).into();
        assert_eq!(export(&base, &env), "export A='it'\\''s'\nexport PATH='/farm/bin:/usr/bin'\nunset RUSTC\n");
    }

    #[test]
    fn the_hook_sees_the_environment_and_its_exports_and_unsets_stick() {
        // The test's own PATH: the nix build sandbox has no /usr/bin or /bin.
        let path = std::env::var("PATH").unwrap();
        let env: BTreeMap<String, String> =
            [("A", "1"), ("RUSTC", "x"), ("PATH", path.as_str())].map(|(k, v)| (k.into(), v.into())).into();
        let out = run_hook("export B=\"$A-2\"\nunset RUSTC\necho noise", &env, Path::new("/")).unwrap();
        assert_eq!(out.get("B").map(String::as_str), Some("1-2"));
        assert!(!out.contains_key("RUSTC"));
        assert!(run_hook("false", &env, Path::new("/")).is_err());
    }
}
