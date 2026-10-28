//! Running independent, read-only work at once: queries that each wait on a process or the
//! network. Commits never go through here; they stay in order under the journal.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Threads per call. The work waits on processes and the network rather than the CPU, so this
/// is above the core count of a small machine, and low enough not to swamp a package database.
const JOBS: usize = 8;

/// `f` over `items`, up to [`JOBS`] at a time, results in item order.
pub fn map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    if items.len() < 2 {
        return items.iter().map(f).collect();
    }
    let next = AtomicUsize::new(0);
    let out: Mutex<Vec<Option<R>>> = Mutex::new(items.iter().map(|_| None).collect());
    std::thread::scope(|s| {
        for _ in 0..JOBS.min(items.len()) {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(i) else { break };
                    let r = f(item);
                    out.lock().unwrap()[i] = Some(r);
                }
            });
        }
    });
    out.into_inner().unwrap().into_iter().map(|r| r.expect("every item ran")).collect()
}

/// [`map`] for fallible work: the first error in item order, or every result.
pub fn try_map<T: Sync, R: Send, E: Send>(items: &[T], f: impl Fn(&T) -> Result<R, E> + Sync) -> Result<Vec<R>, E> {
    map(items, f).into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_item_order_and_runs_at_once() {
        let items: Vec<u64> = (0..40).collect();
        let running = AtomicUsize::new(0);
        let most = AtomicUsize::new(0);
        let out = map(&items, |i| {
            let now = running.fetch_add(1, Ordering::SeqCst) + 1;
            most.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(5));
            running.fetch_sub(1, Ordering::SeqCst);
            i * 2
        });
        assert_eq!(out, items.iter().map(|i| i * 2).collect::<Vec<_>>());
        assert!(most.load(Ordering::SeqCst) > 1);
        assert!(most.load(Ordering::SeqCst) <= JOBS);
    }

    #[test]
    fn try_map_returns_the_first_error() {
        let r: Result<Vec<u32>, u32> = try_map(&[1, 2, 3, 4], |&i| if i % 2 == 0 { Err(i) } else { Ok(i) });
        assert_eq!(r, Err(2));
    }
}
