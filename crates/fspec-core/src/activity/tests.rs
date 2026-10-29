use std::io::{BufRead, BufReader};
use std::time::Instant;

use super::*;

/// A stream directory under a private temporary one, as [`ready`] checks the parent too.
fn stream_dir() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("activity");
    (tmp, dir)
}

fn op() -> Operation {
    Operation::new(Kind::Apply, "g815", "Applying g815", Some("g815"))
}

/// A client reading `path` line by line, failing a test that waits more than five seconds.
fn client(path: &Path) -> impl Iterator<Item = Line> + use<> {
    let stream = UnixStream::connect(path).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    BufReader::new(stream).lines().map(|l| serde_json::from_str(&l.unwrap()).unwrap())
}

fn op_of(line: &Line) -> &Operation {
    match &line.message {
        Message::Snapshot { op } | Message::Update { op } => op,
        other => panic!("not an operation: {other:?}"),
    }
}

#[test]
fn a_client_joining_mid_operation_reads_where_it_is() {
    let (_tmp, dir) = stream_dir();
    let p = Publisher::start(&dir, op()).unwrap();
    p.step(|s| {
        *s = Step {
            phase: Phase::Committing,
            scope: Some(Scope::User),
            progress: Some(Progress { done: 3, total: 12, unit: Unit::Resources }),
            item: Some("zypper: firefox".into()),
            keys: vec!["pkg:firefox".into()],
        }
    });
    let first = client(p.path()).next().unwrap();
    assert_eq!(first.v, VERSION);
    let Message::Snapshot { op } = &first.message else { panic!("{first:?}") };
    assert_eq!(op.step.phase, Phase::Committing);
    assert_eq!(op.step.progress, Some(Progress { done: 3, total: 12, unit: Unit::Resources }));
    assert_eq!(op.state, State::Running);
    p.finish(None);
}

#[test]
fn every_line_is_flat_versioned_json() {
    let line = String::from_utf8(encode(Message::Snapshot { op: op() })).unwrap();
    assert!(line.ends_with('\n') && line.matches('\n').count() == 1, "{line:?}");
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["v"], 1);
    assert_eq!(v["type"], "snapshot");
    assert_eq!(v["op"]["kind"], "apply");
    assert_eq!(v["op"]["source"], "fspec:apply:g815");
    assert_eq!(v["op"]["phase"], "evaluating");
    assert_eq!(v["op"]["state"], "running");
    assert!(v["op"]["progress"].is_null());
}

#[test]
fn the_last_line_is_the_result_and_the_socket_goes() {
    let (_tmp, dir) = stream_dir();
    let p = Publisher::start(&dir, op()).unwrap();
    let path = p.path().to_path_buf();
    let lines = client(&path);
    for done in 1..=5 {
        p.step(|s| s.progress = Some(Progress { done, total: 5, unit: Unit::Resources }));
    }
    p.changed(5);
    p.finish(None);
    let lines: Vec<Line> = lines.collect();
    let last = op_of(lines.last().unwrap());
    assert_eq!(last.state, State::Succeeded);
    assert_eq!(last.result, Some(Outcome { changes: 5, summary: "5 changes".into() }));
    assert_eq!(last.step.progress.unwrap().done, 5);
    assert!(!path.exists());
    assert!(running(&dir).is_empty());
}

#[test]
fn a_failure_carries_its_error_and_no_result() {
    let (_tmp, dir) = stream_dir();
    let p = Publisher::start(&dir, op()).unwrap();
    let lines = client(p.path());
    p.changed(1);
    p.finish(Some("zypper: exit 104".into()));
    let lines: Vec<Line> = lines.collect();
    let last = op_of(lines.last().unwrap());
    assert_eq!(last.state, State::Failed);
    assert_eq!(last.error.as_deref(), Some("zypper: exit 104"));
    assert_eq!(last.result, None);
}

#[test]
fn a_dropped_publisher_ends_as_failed() {
    let (_tmp, dir) = stream_dir();
    let p = Publisher::start(&dir, op()).unwrap();
    let lines = client(p.path());
    drop(p);
    let lines: Vec<Line> = lines.collect();
    assert_eq!(op_of(lines.last().unwrap()).state, State::Failed);
}

/// Updates are coalesced: a burst of steps reaches a client as at most a few lines, the newest
/// state among them.
#[test]
fn a_burst_of_steps_is_sent_as_its_latest_state() {
    let (_tmp, dir) = stream_dir();
    let p = Publisher::start(&dir, op()).unwrap();
    let lines = client(p.path());
    for done in 1..=10_000 {
        p.step(|s| s.progress = Some(Progress { done, total: 10_000, unit: Unit::Packages }));
    }
    p.finish(None);
    let lines: Vec<Line> = lines.collect();
    assert!(lines.len() < 50, "{} lines", lines.len());
    assert_eq!(op_of(lines.last().unwrap()).step.progress.unwrap().done, 10_000);
}

#[test]
fn a_full_client_socket_never_blocks_a_send() {
    let (a, _b) = UnixStream::pair().unwrap();
    a.set_nonblocking(true).unwrap();
    let line = vec![b'x'; 4096];
    let started = Instant::now();
    let mut sent = 0;
    while send(&a, &line) {
        sent += 1;
        assert!(sent < 100_000, "the buffer never filled");
    }
    assert!(started.elapsed() < Duration::from_secs(2));
}

/// A client that never reads cannot slow the steps of the operation, and is cut off once its
/// socket is full rather than kept waiting on.
#[test]
fn a_client_that_never_reads_is_cut_off_not_waited_on() {
    let (_tmp, dir) = stream_dir();
    let p = Publisher::start(&dir, op()).unwrap();
    let stuck = UnixStream::connect(p.path()).unwrap();
    let big = "x".repeat(4000);
    let deadline = Instant::now() + Duration::from_secs(20);
    let (mut n, mut slowest) = (0u64, Duration::ZERO);
    while !p.shared.lock().clients.is_empty() || n == 0 {
        n += 1;
        let started = Instant::now();
        p.step(|s| s.item = Some(format!("{n}{big}")));
        slowest = slowest.max(started.elapsed());
        assert!(Instant::now() < deadline, "the stuck client was never dropped");
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(slowest < Duration::from_millis(100), "a step took {slowest:?}");
    p.finish(None);
    drop(stuck);
}

#[test]
fn a_relay_sets_the_parent_step_and_nothing_else() {
    let (_tmp, dir) = stream_dir();
    let p = Publisher::start(&dir, op()).unwrap();
    let lines = client(p.path());
    let mut relay = Relay::connect(p.path()).unwrap();
    assert!(relay.step(|s| {
        s.phase = Phase::Committing;
        s.scope = Some(Scope::System);
        s.progress = Some(Progress { done: 1, total: 2, unit: Unit::Resources });
    }));
    assert!(send(&relay.stream, &encode(Message::Changed { changes: 2 })));
    let seen = lines
        .map(|l| op_of(&l).clone())
        .find(|op| op.step.scope == Some(Scope::System))
        .expect("the relayed step never arrived");
    assert_eq!(seen.step.phase, Phase::Committing);
    assert_eq!(seen.state, State::Running);
    assert_eq!(seen.id, p.shared.lock().op.id);
    let wait = Instant::now() + Duration::from_secs(5);
    while p.shared.lock().changes != 2 {
        assert!(Instant::now() < wait, "the relayed change count never arrived");
        std::thread::sleep(Duration::from_millis(10));
    }
    p.finish(None);
}

#[test]
fn a_dead_operation_socket_is_swept() {
    let (_tmp, dir) = stream_dir();
    ready(&dir).unwrap();
    let dead = dir.join("dead.sock");
    drop(UnixListener::bind(&dead).unwrap());
    assert!(dead.exists());
    let p = Publisher::start(&dir, op()).unwrap();
    assert!(!dead.exists());
    assert_eq!(running(&dir), vec![p.path().to_path_buf()]);
    p.finish(None);
}

#[test]
fn a_directory_others_can_write_is_refused() {
    let (_tmp, open) = stream_dir();
    std::fs::create_dir(&open).unwrap();
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(Publisher::start(&open, op()).is_err());
}

#[test]
fn a_batch_names_two_and_counts_the_rest() {
    let names = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(batch_label(&names(&["firefox"])), "firefox");
    assert_eq!(batch_label(&names(&["git", "firefox"])), "firefox and git");
    assert_eq!(batch_label(&names(&["c", "a", "b", "d"])), "a, b and 2 more");
}
