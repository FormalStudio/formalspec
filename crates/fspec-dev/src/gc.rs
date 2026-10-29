//! `fspec gc` for project environments: `fspec run` projects and link directories not used within
//! the cutoff, link directories whose project is gone, and the user-store packages no remaining
//! one points into.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Result;
use fspec_core::journal::remove_any;

use crate::{FILE, cache_dir, user_store};

/// The paths deleted.
pub fn run(home: &Path, cutoff: SystemTime) -> Result<Vec<PathBuf>> {
    sweep(&cache_dir(home), &user_store(home), cutoff)
}

fn sweep(cache: &Path, store: &Path, cutoff: SystemTime) -> Result<Vec<PathBuf>> {
    let mut removed = Vec::new();
    let mut used = BTreeSet::new();
    // An `fspec run` project rewrites its dev.star on every run; one unused past the cutoff goes,
    // and its farm with it below.
    for e in fs::read_dir(cache.join("once")).into_iter().flatten().flatten() {
        let used = fs::metadata(e.path().join(FILE)).and_then(|m| m.modified());
        if used.is_err() || used.is_ok_and(|t| t <= cutoff) {
            remove_any(&e.path())?;
            removed.push(e.path());
        }
    }
    for e in fs::read_dir(cache.join("farms")).into_iter().flatten().flatten() {
        let farm = e.path();
        let project = fs::read_to_string(farm.join("project")).unwrap_or_default();
        let recent = fs::metadata(farm.join("project")).and_then(|m| m.modified()).is_ok_and(|t| t > cutoff);
        if recent && Path::new(project.trim()).join(FILE).is_file() {
            // Packages without commands (fonts) are only named in `trees`.
            let trees = fs::read_to_string(farm.join("trees")).unwrap_or_default();
            let links = fs::read_dir(farm.join("bin"))
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|l| fs::read_link(l.path()).ok());
            for target in links.chain(trees.lines().map(PathBuf::from)) {
                if let Ok(rest) = target.strip_prefix(store)
                    && let Some(top) = rest.components().next()
                {
                    used.insert(top.as_os_str().to_owned());
                }
            }
        } else {
            remove_any(&farm)?;
            removed.push(farm);
        }
    }
    for e in fs::read_dir(store).into_iter().flatten().flatten() {
        let name = e.file_name();
        if !used.contains(&name) && name != ".partial" {
            remove_any(&e.path())?;
            removed.push(e.path());
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// A run used before the cutoff goes with its farm and the store package only it linked; a
    /// run used since stays whole.
    #[test]
    fn an_unused_run_goes_with_its_farm_and_store_package() {
        // Explicit directories: `cache_dir` and `user_store` follow XDG_CACHE_HOME and
        // XDG_DATA_HOME, which would point this at the real ones.
        let tmp = tempfile::tempdir().unwrap();
        let (cache, store) = (tmp.path().join("cache"), tmp.path().join("store"));
        let cutoff = SystemTime::now() - Duration::from_secs(3600);
        for (key, age) in [("old", Duration::from_secs(7200)), ("new", Duration::ZERO)] {
            let run = cache.join("once").join(key);
            fs::create_dir_all(&run).unwrap();
            fs::write(run.join(FILE), "dev(packages = [])\n").unwrap();
            fs::File::options()
                .write(true)
                .open(run.join(FILE))
                .unwrap()
                .set_modified(SystemTime::now() - age)
                .unwrap();
            let pkg = store.join(format!("{key}-pkg"));
            fs::create_dir_all(pkg.join("bin")).unwrap();
            let farm = cache.join("farms").join(key);
            fs::create_dir_all(farm.join("bin")).unwrap();
            std::os::unix::fs::symlink(pkg.join("bin/tool"), farm.join("bin/tool")).unwrap();
            fs::write(farm.join("project"), run.display().to_string()).unwrap();
        }
        let removed = sweep(&cache, &store, cutoff).unwrap();
        assert!(!cache.join("once/old").exists() && !cache.join("farms/old").exists());
        assert!(!store.join("old-pkg").exists());
        assert!(cache.join("once/new").exists() && cache.join("farms/new").exists() && store.join("new-pkg").exists());
        assert_eq!(removed.len(), 3, "{removed:?}");
    }
}
