use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::runner::{Cmd, Runner};

/// How to reverse one live mutation. Written to disk before the mutation happens, so a crash
/// mid-apply can be reverted on the next run.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "undo", rename_all = "snake_case")]
pub enum Undo {
    /// Put `backup` back at `path`, or remove `path` when nothing was there before.
    RestorePath { path: PathBuf, backup: Option<PathBuf> },
    /// Move `from` back to `to`, if `from` is still there. Used for app data in and out of the trash.
    Move { from: PathBuf, to: PathBuf },
    /// Reverse a package manager, init or bootc operation.
    Run { cmd: Cmd },
}

/// The undo log for one apply in one scope, at `<state dir>/journal.json`.
#[derive(Debug)]
pub struct Journal {
    path: PathBuf,
    backups: PathBuf,
    generation: u64,
    steps: Vec<Undo>,
}

#[derive(Serialize, Deserialize)]
struct OnDisk {
    generation: u64,
    steps: Vec<Undo>,
}

impl Journal {
    fn paths(state_dir: &Path) -> (PathBuf, PathBuf) {
        (state_dir.join("journal.json"), state_dir.join("journal-backups"))
    }

    /// Start a journal. Fails if a previous apply left one behind; call [`Journal::recover`] first.
    pub fn begin(state_dir: &Path, generation: u64) -> Result<Journal> {
        let (path, backups) = Journal::paths(state_dir);
        if path.exists() {
            bail!("{} exists: a previous apply did not finish", path.display());
        }
        // Backups without a journal are what a crash inside `finish` leaves behind.
        remove_any(&backups)?;
        fs::create_dir_all(&backups).with_context(|| format!("creating {}", backups.display()))?;
        let j = Journal { path, backups, generation, steps: vec![] };
        j.persist()?;
        Ok(j)
    }

    /// Revert whatever an interrupted apply left behind. Returns whether there was anything. An
    /// apply whose `state.json` already reached the journal's generation was committed and only
    /// missed its `finish`, so its journal is dropped instead of rolled back.
    pub fn recover(state_dir: &Path, runner: &dyn Runner) -> Result<bool> {
        let (path, backups) = Journal::paths(state_dir);
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let on_disk: OnDisk = serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        let committed = committed_generation(&state_dir.join("state.json"))?.is_some_and(|g| g >= on_disk.generation);
        let journal = Journal { path, backups, generation: on_disk.generation, steps: on_disk.steps };
        if committed {
            journal.finish()?
        } else {
            journal.rollback(runner)?
        }
        Ok(true)
    }

    /// Synced before it returns, since the next step only runs once its undo is on disk. On vfat
    /// (a journal on the ESP) fsync of the directory does not persist the rename, so the whole
    /// filesystem is synced.
    fn persist(&self) -> Result<()> {
        use std::io::Write;
        let tmp = self.path.with_extension("json.tmp");
        let on_disk = OnDisk { generation: self.generation, steps: self.steps.clone() };
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(&on_disk)?)?;
        f.sync_all()?;
        fs::rename(&tmp, &self.path)?;
        let dir = fs::File::open(self.path.parent().unwrap_or(Path::new(".")))?;
        dir.sync_all()?;
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            if unsafe { libc::syncfs(dir.as_raw_fd()) } != 0 {
                return Err(std::io::Error::last_os_error()).context("syncing the journal's filesystem");
            }
        }
        Ok(())
    }

    /// Record an undo step. Call before making the change it reverses.
    pub fn record(&mut self, undo: Undo) -> Result<()> {
        self.steps.push(undo);
        self.persist()
    }

    /// Save whatever is at `path` (file, symlink or directory) so a rollback restores it, and
    /// record the step. Call before writing, replacing or removing `path`.
    pub fn backup(&mut self, path: &Path) -> Result<()> {
        let backup = match fs::symlink_metadata(path) {
            Err(e) if e.kind() == ErrorKind::NotFound => None,
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
            Ok(_) => {
                let dest = self.backups.join(self.steps.len().to_string());
                copy_any(path, &dest).with_context(|| format!("backing up {}", path.display()))?;
                Some(dest)
            }
        };
        self.record(Undo::RestorePath { path: path.into(), backup })
    }

    /// Replay the steps backwards. Keeps going past a failing step and reports all of them.
    pub fn rollback(self, runner: &dyn Runner) -> Result<()> {
        let mut errors = Vec::new();
        for step in self.steps.iter().rev() {
            if let Err(e) = undo(step, runner) {
                errors.push(format!("{e:#}"));
            }
        }
        self.finish()?;
        if !errors.is_empty() {
            bail!("rollback left {} step(s) undone:\n  {}", errors.len(), errors.join("\n  "));
        }
        Ok(())
    }

    /// The apply succeeded: drop the journal and its backups. The journal goes first, since a
    /// journal whose backups are gone would make `recover` delete the live files.
    pub fn finish(self) -> Result<()> {
        remove_any(&self.path)?;
        remove_any(&self.backups)
    }
}

/// The generation in the `state.json` at `path`, `None` when there is none.
fn committed_generation(path: &Path) -> Result<Option<u64>> {
    #[derive(Deserialize)]
    struct Generation {
        #[serde(default)]
        generation: u64,
    }
    match fs::read(path) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice::<Generation>(&bytes)
                .with_context(|| format!("parsing {}", path.display()))?
                .generation,
        )),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn undo(step: &Undo, runner: &dyn Runner) -> Result<()> {
    match step {
        Undo::RestorePath { path, backup } => {
            if let Some(b) = backup.as_ref().filter(|b| fs::symlink_metadata(b).is_err()) {
                bail!("not restoring {}: its backup {} is missing", path.display(), b.display());
            }
            remove_any(path)?;
            if let Some(b) = backup {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                copy_any(b, path).with_context(|| format!("restoring {}", path.display()))?;
            }
            Ok(())
        }
        Undo::Move { from, to } => {
            if fs::symlink_metadata(from).is_ok() {
                move_any(from, to)?;
            }
            Ok(())
        }
        Undo::Run { cmd } => runner.check(cmd).map(drop),
    }
}

/// Rename `from` to `to`, copying across filesystems when a rename cannot.
pub fn move_any(from: &Path, to: &Path) -> Result<()> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    if fs::rename(from, to).is_ok() {
        return Ok(());
    }
    copy_any(from, to).and_then(|()| remove_any(from)).with_context(|| format!("moving {}", from.display()))
}

/// Copy a file, symlink (as a symlink) or directory tree, preserving permissions.
pub fn copy_any(from: &Path, to: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(from)?;
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    if meta.file_type().is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(from)?, to)?;
    } else if meta.is_dir() {
        fs::create_dir_all(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            // A supervisor renames its status files in place, so an entry can vanish mid-copy.
            match copy_any(&entry.path(), &to.join(entry.file_name())) {
                Err(e) if e.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == ErrorKind::NotFound) => {}
                r => r?,
            }
        }
        fs::set_permissions(to, meta.permissions())?;
    } else if meta.is_file() {
        fs::copy(from, to)?;
    }
    // FIFOs and sockets, like the `supervise/` pipes in a running runit or s6 service directory,
    // belong to the supervisor, which recreates them.
    Ok(())
}

/// Remove a file, symlink or directory tree; a missing path is fine.
pub fn remove_any(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
        Ok(m) if m.is_dir() => fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display())),
        Ok(_) => fs::remove_file(path).with_context(|| format!("removing {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::RecordingRunner;

    #[test]
    fn rollback_restores_files_and_runs_commands() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let existing = dir.path().join("etc/existing");
        let created = dir.path().join("etc/created");
        fs::create_dir_all(existing.parent().unwrap()).unwrap();
        fs::write(&existing, "old").unwrap();

        let mut j = Journal::begin(&state, 1).unwrap();
        j.backup(&existing).unwrap();
        fs::write(&existing, "new").unwrap();
        j.backup(&created).unwrap();
        fs::write(&created, "new").unwrap();
        j.record(Undo::Run { cmd: Cmd::new("flatpak").args(["uninstall", "-y", "x"]) }).unwrap();

        let runner = RecordingRunner::default();
        // A crash here leaves the journal on disk; the next run recovers from it.
        drop(j);
        assert!(Journal::begin(&state, 2).is_err());
        assert!(Journal::recover(&state, &runner).unwrap());
        assert_eq!(fs::read_to_string(&existing).unwrap(), "old");
        assert!(!created.exists());
        assert_eq!(runner.lines(), ["flatpak uninstall -y x"]);
        assert!(!state.join("journal.json").exists());
        assert!(!Journal::recover(&state, &runner).unwrap());
    }

    #[test]
    fn recover_keeps_an_apply_whose_state_was_saved() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let file = dir.path().join("etc/file");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "old").unwrap();

        let mut j = Journal::begin(&state, 3).unwrap();
        j.backup(&file).unwrap();
        fs::write(&file, "new").unwrap();
        fs::write(state.join("state.json"), r#"{"version":1,"generation":3}"#).unwrap();
        // A crash after the state is saved, before `finish` drops the journal.
        drop(j);
        let runner = RecordingRunner::default();
        assert!(Journal::recover(&state, &runner).unwrap());
        assert_eq!(fs::read_to_string(&file).unwrap(), "new");
        assert!(!state.join("journal.json").exists() && !state.join("journal-backups").exists());
    }

    #[test]
    fn rollback_keeps_a_file_whose_backup_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let file = dir.path().join("etc/file");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "old").unwrap();

        let mut j = Journal::begin(&state, 1).unwrap();
        j.backup(&file).unwrap();
        fs::write(&file, "new").unwrap();
        remove_any(&state.join("journal-backups")).unwrap();
        drop(j);
        assert!(Journal::recover(&state, &RecordingRunner::default()).is_err());
        assert_eq!(fs::read_to_string(&file).unwrap(), "new");
    }

    #[test]
    fn backup_skips_fifos_in_a_service_directory() {
        let dir = tempfile::tempdir().unwrap();
        let svc = dir.path().join("sv/web");
        fs::create_dir_all(svc.join("supervise")).unwrap();
        fs::write(svc.join("run"), "#!/bin/sh\n").unwrap();
        let fifo = svc.join("supervise/control");
        assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());

        let mut j = Journal::begin(&dir.path().join("state"), 1).unwrap();
        j.backup(&svc).unwrap();
        remove_any(&svc).unwrap();
        j.rollback(&RecordingRunner::default()).unwrap();
        assert_eq!(fs::read_to_string(svc.join("run")).unwrap(), "#!/bin/sh\n");
        assert!(svc.join("supervise").is_dir() && !fifo.exists());
    }
}
