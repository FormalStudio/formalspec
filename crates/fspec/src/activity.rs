//! `fspec activity`: the operation stream (`docs/activity.md`) of every apply, update and
//! rollback running as this user, printed for people or, with `--json`, passed through.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use fspec_core::activity::{Line, Message, Operation, State, VERSION, running};
use notify::Watcher;

/// How often the directory is listed when the watch has said nothing.
const RESCAN: Duration = Duration::from_secs(2);

pub fn print(dir: &Path, json: bool, once: bool) -> Result<()> {
    if once {
        let mut any = false;
        for sock in running(dir) {
            if let Some(line) = connect(&sock).and_then(|mut lines| lines.next()) {
                emit(&line, json);
                any = true;
            }
        }
        if !any && !json {
            println!("no fspec operation is running");
        }
        return Ok(());
    }
    // The directory is watched, as a shell would, so an operation shorter than a listing
    // interval is still caught; the timeout is the fallback for a watch that drops events.
    fspec_core::activity::ready(dir)?;
    let (tx, rx) = std::sync::mpsc::channel::<Option<PathBuf>>();
    let wake = tx.clone();
    let mut watcher = notify::recommended_watcher(move |_: notify::Result<notify::Event>| {
        let _ = wake.send(None);
    })
    .context("watching the activity directory")?;
    watcher.watch(dir, notify::RecursiveMode::NonRecursive)?;
    let mut following: BTreeSet<PathBuf> = BTreeSet::new();
    loop {
        let now = running(dir);
        following.retain(|p| now.contains(p));
        for sock in now {
            if following.insert(sock.clone()) {
                let done = tx.clone();
                std::thread::spawn(move || {
                    let (mut read, mut ended) = (false, false);
                    for line in connect(&sock).into_iter().flatten() {
                        (read, ended) = (true, ended || is_end(&line));
                        emit(&line, json);
                    }
                    // Cut off before the end, it reconnects for a snapshot while the socket is
                    // there. A socket that never answered is a dead one, left to the next sweep.
                    if read && !ended {
                        let _ = done.send(Some(sock));
                    }
                });
            }
        }
        if let Ok(Some(cut)) = rx.recv_timeout(RESCAN) {
            following.remove(&cut);
        }
    }
}

/// The complete lines of the operation at `sock`, ending where it does. A last line with no
/// newline was cut short by the operation dropping this reader, and is not one.
fn connect(sock: &Path) -> Option<impl Iterator<Item = String> + use<>> {
    let mut reader = BufReader::new(UnixStream::connect(sock).ok()?);
    Some(std::iter::from_fn(move || {
        let mut line = Vec::new();
        match reader.read_until(b'\n', &mut line) {
            Ok(_) if line.pop() == Some(b'\n') => String::from_utf8(line).ok(),
            _ => None,
        }
    }))
}

fn is_end(line: &str) -> bool {
    matches!(
        serde_json::from_str::<Line>(line),
        Ok(Line { message: Message::Snapshot { op } | Message::Update { op }, .. }) if op.state != State::Running
    )
}

fn emit(line: &str, json: bool) {
    let mut out = std::io::stdout().lock();
    let _ = match json {
        true => writeln!(out, "{line}"),
        false => match serde_json::from_str::<Line>(line) {
            Ok(Line { v: VERSION, message: Message::Snapshot { op } | Message::Update { op } }) => {
                writeln!(out, "{}", describe(&op))
            }
            _ => Ok(()),
        },
    };
    let _ = out.flush();
}

/// `Applying g815: committing system, 3/12 resources, zypper: firefox and git`.
pub fn describe(op: &Operation) -> String {
    match op.state {
        State::Succeeded => {
            format!("{}: done, {}", op.title, op.result.as_ref().map_or("finished", |r| r.summary.as_str()))
        }
        State::Failed => format!("{}: failed: {}", op.title, op.error.as_deref().unwrap_or("no reason given")),
        State::Running => {
            let mut s = format!("{}: {}", op.title, name(&op.step.phase));
            if let Some(scope) = &op.step.scope {
                s += &format!(" {}", name(scope));
            }
            if let Some(p) = &op.step.progress {
                s += &format!(", {}/{} {}", p.done, p.total, name(&p.unit));
            }
            if let Some(item) = &op.step.item {
                s += &format!(", {item}");
            }
            s
        }
    }
}

/// The wire name of an enum value, as serde writes it.
fn name(v: &impl serde::Serialize) -> String {
    serde_json::to_value(v).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use fspec_core::activity::{Kind, Outcome, Phase, Progress, Step, Unit};
    use fspec_core::resource::Scope;

    use super::*;

    #[test]
    fn a_line_cut_short_at_the_end_is_dropped() {
        use std::io::Write as _;
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("op.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            s.write_all(b"{\"v\":1}\n{\"v\":1,\"ty").unwrap();
        });
        let lines: Vec<String> = connect(&sock).unwrap().collect();
        server.join().unwrap();
        assert_eq!(lines, vec![r#"{"v":1}"#.to_string()]);
    }

    #[test]
    fn a_running_apply_reads_as_one_line() {
        let mut op = Operation::new(Kind::Apply, "g815", "Applying g815", Some("g815"));
        assert_eq!(describe(&op), "Applying g815: evaluating");
        op.step = Step {
            phase: Phase::Committing,
            scope: Some(Scope::System),
            progress: Some(Progress { done: 3, total: 12, unit: Unit::Resources }),
            item: Some("zypper: firefox and git".into()),
            keys: vec![],
        };
        assert_eq!(describe(&op), "Applying g815: committing system, 3/12 resources, zypper: firefox and git");
        op.state = State::Succeeded;
        op.result = Some(Outcome { changes: 12, summary: "12 changes".into() });
        assert_eq!(describe(&op), "Applying g815: done, 12 changes");
    }
}
