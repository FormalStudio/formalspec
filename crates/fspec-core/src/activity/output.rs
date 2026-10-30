//! A long resource's inner step, read off its command's output lines: what phase a build is in,
//! how far cargo is, and whether the output has gone quiet.

use std::time::{Duration, Instant};

use super::{InnerStep, Progress, Unit};

/// No line for this long and the step reads as stalled.
pub const STALL: Duration = Duration::from_secs(60);
/// The shortest gap between two published steps that differ only in progress.
pub const THROTTLE: Duration = Duration::from_millis(250);
/// Set on a build's commands so cargo draws its bar on a pipe, which [`classify`] reads.
pub const CARGO_ENV: [(&str, &str); 2] = [("CARGO_TERM_PROGRESS_WHEN", "always"), ("CARGO_TERM_PROGRESS_WIDTH", "200")];

pub const FETCHING: &str = "fetching dependencies";
pub const INSTALLING: &str = "installing dependencies";
pub const COMPILING: &str = "compiling";
pub const PACKAGING: &str = "packaging";

/// What one output line says, if it says anything: a phase name and, for cargo's bar, how many
/// units of how many are built.
pub fn classify(line: &str) -> Option<(&'static str, Option<(u64, u64)>)> {
    let line = strip_ansi(line);
    let line = line.trim();
    if let Some(bar) = line.strip_prefix("Building [") {
        return Some((COMPILING, bar.split_once("] ").and_then(|(_, rest)| fraction(rest))));
    }
    let starts = |prefixes: &[&str]| prefixes.iter().any(|p| line.starts_with(p));
    if starts(&["Compiling ", "Checking "]) {
        return Some((COMPILING, None));
    }
    if starts(&[
        "Get:",
        "Hit:",
        "Ign:",
        "Err:",
        "Fetched ",
        "Reading package lists",
        "Building dependency tree",
        "Reading state information",
        "Need to get ",
        "Updating crates.io index",
        "Updating git repository",
        "Updating git submodule",
        "Updating index",
        "Downloading ",
        "Downloaded ",
        "Locking ",
        "Fetching ",
        "From ",
        "remote: ",
        "Receiving objects",
        "Resolving deltas",
        "info: downloading",
        "info: syncing channel",
    ]) {
        return Some((FETCHING, None));
    }
    if starts(&[
        "Selecting previously unselected",
        "Preparing to unpack",
        "Unpacking ",
        "Setting up ",
        "Processing triggers",
        "info: installing component",
    ]) {
        return Some((INSTALLING, None));
    }
    None
}

/// `120/450: crate, ...` as (120, 450).
fn fraction(s: &str) -> Option<(u64, u64)> {
    let (done, rest) = s.split_once('/')?;
    let total: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let (done, total) = (done.trim().parse().ok()?, total.parse().ok()?);
    (total > 0 && done <= total).then_some((done, total))
}

/// The line without its CSI escapes (cargo's erase-line before the next status line).
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// The inner step of one running command, fed its lines and a tick about once a second. Each
/// call answers the step to publish, or `None` when nothing a reader needs has changed.
pub struct Watch {
    step: InnerStep,
    sent: Option<InnerStep>,
    sent_at: Option<Instant>,
    last_line: Instant,
}

impl Watch {
    pub fn new(name: &str, now: Instant) -> Watch {
        Watch {
            step: InnerStep { name: name.to_string(), progress: None, stalled: false },
            sent: None,
            sent_at: None,
            last_line: now,
        }
    }

    /// Enter `name` from outside the output (`packaging` once the commands are done).
    pub fn enter(&mut self, name: &str, now: Instant) -> Option<InnerStep> {
        if self.step.name != name {
            self.step.name = name.to_string();
            self.step.progress = None;
        }
        self.last_line = now;
        self.step.stalled = false;
        self.publish(now)
    }

    pub fn line(&mut self, line: &str, now: Instant) -> Option<InnerStep> {
        self.last_line = now;
        self.step.stalled = false;
        if let Some((name, progress)) = classify(line) {
            if self.step.name != name {
                self.step.name = name.to_string();
                self.step.progress = None;
            }
            if let Some((done, total)) = progress {
                self.step.progress = Some(Progress { done, total, unit: Unit::Units });
            }
        }
        self.publish(now)
    }

    pub fn tick(&mut self, now: Instant) -> Option<InnerStep> {
        if now.duration_since(self.last_line) >= STALL {
            self.step.stalled = true;
        }
        self.publish(now)
    }

    /// A new name or a stall flip goes out at once; a moved bar at most once per [`THROTTLE`],
    /// the rest of it on a later line or tick.
    fn publish(&mut self, now: Instant) -> Option<InnerStep> {
        let urgent = match &self.sent {
            None => true,
            Some(sent) if *sent == self.step => return None,
            Some(sent) => sent.name != self.step.name || sent.stalled != self.step.stalled,
        };
        if !urgent && self.sent_at.is_some_and(|at| now.duration_since(at) < THROTTLE) {
            return None;
        }
        self.sent = Some(self.step.clone());
        self.sent_at = Some(now);
        self.sent.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn units(done: u64, total: u64) -> Option<Progress> {
        Some(Progress { done, total, unit: Unit::Units })
    }

    #[test]
    fn cargo_bar_segments_read_as_compiling_with_a_fraction() {
        let raw = "    Building [=====>          ] 120/450: serde, tokio(build)\r    Building [======>         ] 121/450: tokio\r";
        let got: Vec<_> = raw.split('\r').filter_map(classify).collect();
        assert_eq!(got, vec![(COMPILING, Some((120, 450))), (COMPILING, Some((121, 450)))]);
    }

    #[test]
    fn a_compiling_line_after_an_erase_is_compiling() {
        assert_eq!(classify("\x1b[K   Compiling formalshell-rs v0.1.0 (/src/crates)"), Some((COMPILING, None)));
    }

    #[test]
    fn apt_lines_are_dependencies_with_no_fraction() {
        assert_eq!(
            classify("Get:12 http://deb.debian.org/debian bookworm/main amd64 curl 7.88.1"),
            Some((FETCHING, None))
        );
        assert_eq!(classify("Hit:1 http://deb.debian.org/debian bookworm InRelease"), Some((FETCHING, None)));
        assert_eq!(classify("Unpacking git (1:2.39.5-0+deb12u1) ..."), Some((INSTALLING, None)));
        assert_eq!(classify("Setting up curl (7.88.1-10+deb12u8) ..."), Some((INSTALLING, None)));
        assert_eq!(classify("Building dependency tree..."), Some((FETCHING, None)));
        assert_eq!(classify("    Updating crates.io index"), Some((FETCHING, None)));
        assert_eq!(classify("linkage bin/formalshell: glibc 2.34"), None);
    }

    #[test]
    fn the_bar_moves_progress_and_a_compiling_line_keeps_it() {
        let t0 = Instant::now();
        let mut w = Watch::new("building", t0);
        let first = w.line("Get:1 http://deb.debian.org/debian bookworm InRelease", t0).unwrap();
        assert_eq!((first.name.as_str(), first.progress), (FETCHING, None));
        let bar = w.line("    Building [==>  ] 3/10: a", t0 + Duration::from_secs(1)).unwrap();
        assert_eq!((bar.name.as_str(), bar.progress), (COMPILING, units(3, 10)));
        let kept = w.line("   Compiling b v1.0.0", t0 + Duration::from_secs(2));
        assert_eq!(kept, None, "nothing changed");
        let back = w.line("Get:2 http://x y", t0 + Duration::from_secs(3)).unwrap();
        assert_eq!(back.progress, None, "a new phase drops the last one's fraction");
    }

    #[test]
    fn bar_updates_are_throttled_and_flushed_on_a_tick() {
        let t0 = Instant::now();
        let mut w = Watch::new("building", t0);
        assert!(w.line("Building [>] 1/100: a", t0).is_some());
        let soon = t0 + Duration::from_millis(50);
        assert_eq!(w.line("Building [>] 2/100: a", soon), None);
        assert_eq!(w.line("Building [>] 3/100: a", soon), None);
        let later = t0 + THROTTLE;
        assert_eq!(w.tick(later).unwrap().progress, units(3, 100));
    }

    #[test]
    fn silence_flips_stalled_once_and_a_line_flips_it_back() {
        let t0 = Instant::now();
        let mut w = Watch::new("building", t0);
        assert!(w.line("Get:1 http://x y", t0).is_some());
        assert_eq!(w.tick(t0 + STALL - Duration::from_secs(1)), None);
        let stalled = w.tick(t0 + STALL).unwrap();
        assert!(stalled.stalled);
        assert_eq!(stalled.name, FETCHING);
        assert_eq!(w.tick(t0 + STALL + Duration::from_secs(1)), None, "the flip is sent once");
        // Within the throttle of the stall, the flip back still goes out.
        let back = w.line("Get:2 http://x y", t0 + STALL + Duration::from_millis(1001)).unwrap();
        assert!(!back.stalled);
    }
}
