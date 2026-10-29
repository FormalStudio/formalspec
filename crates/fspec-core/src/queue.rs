//! The queue `fspec apply` waits in per host, on the machine running it. Applies of one host run
//! one at a time and read the spec only once they are at the front, so a queued apply never
//! converges an older spec over a newer one. An apply that queued while another was waiting
//! is skipped when the one that ran after it read the same spec later and succeeded.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::flock;

/// What one apply was asked to converge.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub spec_dir: PathBuf,
    /// `--rev` as given (`HEAD`, a branch, a commit), or `None` for the working tree.
    pub rev: Option<String>,
    /// `--scope`, or `None` for both.
    pub scope: Option<String>,
}

/// The last apply of a host that ran, in `<dir>/<host>.json`.
#[derive(Serialize, Deserialize, Debug)]
struct Run {
    pid: u32,
    /// Nanoseconds since the epoch at which it reached the front of the queue and read the spec.
    started: u128,
    request: Request,
    ok: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Ran,
    /// An apply that read the spec after this one queued converged the same request.
    Covered {
        pid: u32,
    },
}

/// Run `apply` for `host` at the front of its queue in `dir`. `apply` is told whether this one
/// waited, so it knows a spec it read before queueing may be stale.
pub fn run(
    dir: &Path,
    host: &str,
    request: &Request,
    wait: bool,
    apply: impl FnOnce(bool) -> Result<()>,
) -> Result<Outcome> {
    let queued = now();
    let held = flock::acquire(&dir.join(format!("{host}.lock")), &format!("apply of host {host}"), wait)?;
    let record = dir.join(format!("{host}.json"));
    if held.waited
        && let Some(last) = std::fs::read(&record).ok().and_then(|b| serde_json::from_slice::<Run>(&b).ok())
        && covers(&last, request, queued)
    {
        eprintln!(
            "fspec: host {host}: apply pid {} read the same spec after this one queued and converged it; nothing left to do",
            last.pid
        );
        return Ok(Outcome::Covered { pid: last.pid });
    }
    let started = now();
    let result = apply(held.waited);
    let run = Run { pid: std::process::id(), started, request: request.clone(), ok: result.is_ok() };
    flock::write_atomic(&record, &serde_json::to_vec(&run)?)
        .with_context(|| format!("recording the apply of {host}"))?;
    result.map(|()| Outcome::Ran)
}

fn covers(last: &Run, request: &Request, queued: u128) -> bool {
    last.ok
        && last.started > queued
        && last.request.spec_dir == request.spec_dir
        && last.request.rev == request.rev
        && (last.request.scope.is_none() || last.request.scope == request.scope)
}

fn now() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    fn request(rev: Option<&str>) -> Request {
        Request { spec_dir: "/spec".into(), rev: rev.map(str::to_string), scope: None }
    }

    /// The first apply holds the queue while two more join it; the second runs, the third finds
    /// the second read the spec after it queued and is skipped.
    #[test]
    fn of_three_queued_applies_the_last_is_covered_by_the_one_before_it() {
        let dir = tempfile::tempdir().unwrap();
        let (go, wait_go) = mpsc::channel::<()>();
        let first = {
            let dir = dir.path().to_path_buf();
            std::thread::spawn(move || {
                run(&dir, "h", &request(None), true, |_| {
                    wait_go.recv().unwrap();
                    Ok(())
                })
            })
        };
        std::thread::sleep(Duration::from_millis(100));
        let queued = |rev: Option<&'static str>| {
            let dir = dir.path().to_path_buf();
            std::thread::spawn(move || {
                run(&dir, "h", &request(rev), true, |waited| {
                    assert!(waited);
                    std::thread::sleep(Duration::from_millis(400));
                    Ok(())
                })
            })
        };
        let second = queued(None);
        let third = queued(None);
        std::thread::sleep(Duration::from_millis(100));
        go.send(()).unwrap();
        assert_eq!(first.join().unwrap().unwrap(), Outcome::Ran);
        let outcomes = [second.join().unwrap().unwrap(), third.join().unwrap().unwrap()];
        assert_eq!(outcomes.iter().filter(|o| **o == Outcome::Ran).count(), 1, "{outcomes:?}");
        assert!(outcomes.iter().any(|o| matches!(o, Outcome::Covered { .. })), "{outcomes:?}");
    }

    #[test]
    fn a_failed_or_different_apply_covers_nothing() {
        let req = request(None);
        let last = |ok, rev: Option<&str>, started| Run { pid: 1, started, request: request(rev), ok };
        assert!(covers(&last(true, None, 10), &req, 5));
        assert!(!covers(&last(false, None, 10), &req, 5));
        assert!(!covers(&last(true, Some("HEAD"), 10), &req, 5));
        assert!(!covers(&last(true, None, 4), &req, 5));
        let scoped = Request { scope: Some("user".into()), ..req.clone() };
        assert!(covers(&last(true, None, 10), &scoped, 5));
        let both = Run { request: scoped.clone(), ..last(true, None, 10) };
        assert!(!covers(&both, &req, 5));
    }
}
