//! Advisory locks that agents and people running fspec at once queue on. The holder writes who it
//! is into the lock file, so a waiter can say what it is waiting for.

use std::fs;
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

/// A held lock, released when dropped.
#[derive(Debug)]
pub struct Held {
    file: fs::File,
    /// Whether another process held the lock when it was asked for.
    pub waited: bool,
}

impl Drop for Held {
    /// Closing is not enough: a child spawned on another thread holds a copy of every fd until its
    /// exec closes the CLOEXEC ones, and the flock lives until the last copy goes.
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// How often a waiter tries again, and how often it says it is still waiting.
const POLL: Duration = Duration::from_millis(200);
const REPORT: Duration = Duration::from_secs(30);

/// Take the lock at `path` for the life of the returned guard. `what` names the work it guards
/// in messages ("apply of host g815"). Another holder makes it wait, saying who holds it and for
/// how long, or fail at once when `wait` is false.
pub fn acquire(path: &Path, what: &str, wait: bool) -> Result<Held> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let mut waited = false;
    let started = Instant::now();
    let mut reported: Option<Instant> = None;
    loop {
        match file.try_lock() {
            Ok(()) => break,
            Err(fs::TryLockError::WouldBlock) => {}
            Err(fs::TryLockError::Error(e)) => return Err(e).with_context(|| format!("locking {}", path.display())),
        }
        let holder = holder(&mut file);
        if !wait {
            bail!("another {what} ({holder}) holds {}; drop --no-wait to queue behind it", path.display());
        }
        if reported.is_none_or(|t| t.elapsed() >= REPORT) {
            match reported {
                None => eprintln!("fspec: waiting for another {what} ({holder})"),
                Some(_) => eprintln!("fspec: still waiting for {what} ({holder}), {} so far", age(started.elapsed())),
            }
            reported = Some(Instant::now());
        }
        waited = true;
        std::thread::sleep(POLL);
    }
    if waited {
        eprintln!("fspec: got {what} after {}", age(started.elapsed()));
    }
    file.set_len(0)?;
    file.rewind()?;
    write!(file, "{} {} {what}", std::process::id(), now_secs())?;
    Ok(Held { file, waited })
}

/// `pid N, running 2m10s`, from what the holder wrote.
fn holder(file: &mut fs::File) -> String {
    let mut text = String::new();
    let _ = file.rewind().and_then(|_| file.read_to_string(&mut text));
    let mut words = text.split_whitespace();
    let pid = words.next().unwrap_or("?");
    match words.next().and_then(|s| s.parse::<u64>().ok()) {
        Some(since) => format!("pid {pid}, running {}", age(Duration::from_secs(now_secs().saturating_sub(since)))),
        None => format!("pid {pid}"),
    }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default()
}

fn age(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
    }
}

/// Write `bytes` to `path` through a uniquely named temporary file beside it and a rename, so a
/// crash never leaves a torn file and two writers never share a temporary.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let name = path.file_name().context("path has no file name")?.to_string_lossy();
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let tmp: PathBuf = dir.join(format!(".{name}.{}-{n}.tmp", std::process::id()));
    fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("renaming {} into place", tmp.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_holder_is_refused_without_wait_and_named() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.lock");
        let first = acquire(&path, "apply of host a", true).unwrap();
        assert!(!first.waited);
        let err = acquire(&path, "apply of host a", false).unwrap_err().to_string();
        assert!(err.contains(&format!("another apply of host a (pid {}, running", std::process::id())), "{err}");
        drop(first);
        acquire(&path, "apply of host a", false).unwrap();
    }

    #[test]
    fn a_waiter_gets_the_lock_once_the_holder_lets_go() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.lock");
        let first = acquire(&path, "build", true).unwrap();
        let waiter = {
            let path = path.clone();
            std::thread::spawn(move || acquire(&path, "build", true).map(|h| h.waited))
        };
        std::thread::sleep(Duration::from_millis(300));
        assert!(!waiter.is_finished());
        drop(first);
        assert!(waiter.join().unwrap().unwrap());
    }

    #[test]
    fn ages_read_short() {
        assert_eq!(age(Duration::from_secs(5)), "5s");
        assert_eq!(age(Duration::from_secs(130)), "2m10s");
        assert_eq!(age(Duration::from_secs(3720)), "1h02m");
    }
}
