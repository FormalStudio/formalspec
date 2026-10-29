//! The operation stream: what a running `apply`, `update` or `rollback` is doing, for a desktop
//! shell to show while it runs. `docs/activity.md` is the protocol.
//!
//! Each operation listens on its own socket in [`dir`] for as long as it runs. A client gets the
//! whole operation on connect and again after every change, at most once per [`INTERVAL`].
//! Nothing here can hold an apply up: a step only changes the state under a mutex, a broadcaster
//! thread does the writing without blocking, and a client that does not keep up is cut off, to
//! come back for a fresh snapshot.
//!
//! The system scope runs in a `sudo fspec` child. It does not listen itself: it connects to the
//! parent's socket as a relay and sends its steps, which the parent publishes as its own.

use std::collections::BTreeSet;
use std::io::{ErrorKind, Read};
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::resource::Scope;

/// The protocol version every line carries in `v`.
pub const VERSION: u32 = 1;
/// The shortest gap between two updates a client is sent.
pub const INTERVAL: Duration = Duration::from_millis(50);
/// `keys` is cut to this many.
pub const MAX_KEYS: usize = 32;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Apply,
    Update,
    Rollback,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Running,
    Succeeded,
    Failed,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    #[default]
    Evaluating,
    Queued,
    Remote,
    Planning,
    Preparing,
    Committing,
    Resolving,
    Writing,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Unit {
    Resources,
    Packages,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Progress {
    pub done: u64,
    pub total: u64,
    pub unit: Unit,
}

/// What the operation is doing now: the part of it a relay may set.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
pub struct Step {
    pub phase: Phase,
    pub scope: Option<Scope>,
    /// `None` while the phase cannot say how far it is.
    pub progress: Option<Progress>,
    pub item: Option<String>,
    pub keys: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub changes: u64,
    pub summary: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Operation {
    pub id: String,
    pub kind: Kind,
    /// Equal for operations a shell shows as one row: applies of one host, updates of one spec.
    pub source: String,
    pub title: String,
    pub host: Option<String>,
    pub pid: u32,
    /// Milliseconds since the epoch.
    pub started: u64,
    pub updated: u64,
    pub state: State,
    #[serde(flatten)]
    pub step: Step,
    pub result: Option<Outcome>,
    pub error: Option<String>,
}

impl Operation {
    /// An operation of this process starting now. `subject` is the host for an apply or rollback
    /// and the spec directory for an update.
    pub fn new(kind: Kind, subject: &str, title: impl Into<String>, host: Option<&str>) -> Operation {
        let started = now_ms();
        let pid = std::process::id();
        let kind_name = match kind {
            Kind::Apply => "apply",
            Kind::Update => "update",
            Kind::Rollback => "rollback",
        };
        Operation {
            id: format!("{started:x}-{pid}"),
            kind,
            source: format!("fspec:{kind_name}:{subject}"),
            title: title.into(),
            host: host.map(str::to_string),
            pid,
            started,
            updated: started,
            state: State::Running,
            step: Step::default(),
            result: None,
            error: None,
        }
    }
}

/// One line of the stream.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Line {
    pub v: u32,
    #[serde(flatten)]
    pub message: Message,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    /// The first line a client reads.
    Snapshot {
        op: Operation,
    },
    Update {
        op: Operation,
    },
    /// Sent by a `sudo fspec` child once, before its steps.
    Relay,
    Step {
        step: Step,
    },
    /// From a relay: `changes` more changes to count in the parent's result.
    Changed {
        changes: u64,
    },
}

fn encode(message: Message) -> Vec<u8> {
    let mut line = serde_json::to_vec(&Line { v: VERSION, message }).unwrap_or_default();
    line.push(b'\n');
    line
}

/// The directory operations listen in: `$XDG_RUNTIME_DIR/fspec/activity`, else
/// `<temp dir>/fspec-<uid>/activity` (macOS has no runtime directory).
pub fn dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        Some(run) => PathBuf::from(run).join("fspec").join("activity"),
        // SAFETY: getuid has no preconditions.
        None => std::env::temp_dir().join(format!("fspec-{}", unsafe { libc::getuid() })).join("activity"),
    }
}

/// Create `dir` and its parent private to this user, and refuse one anybody else could write.
/// A reader creates it too, so it has a directory to watch before any operation ran.
pub fn ready(dir: &Path) -> Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    for d in [dir, dir.parent().unwrap_or(dir)] {
        let meta = std::fs::metadata(d).with_context(|| format!("reading {}", d.display()))?;
        if meta.uid() != uid || meta.permissions().mode() & 0o022 != 0 {
            bail!("{} is not private to uid {uid}", d.display());
        }
    }
    Ok(())
}

/// The sockets of the operations running now, oldest first.
pub fn running(dir: &Path) -> Vec<PathBuf> {
    let Ok(read) = std::fs::read_dir(dir) else { return vec![] };
    let mut socks: Vec<PathBuf> =
        read.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "sock")).collect();
    socks.sort();
    socks
}

/// Remove the sockets of operations whose process is gone.
fn sweep(dir: &Path) {
    for sock in running(dir) {
        if let Err(e) = UnixStream::connect(&sock)
            && e.kind() == ErrorKind::ConnectionRefused
        {
            let _ = std::fs::remove_file(&sock);
        }
    }
}

struct Shared {
    inner: Mutex<Inner>,
    changed: Condvar,
}

struct Inner {
    op: Operation,
    /// Bumped by every change; the broadcaster sends when it moved.
    seq: u64,
    closed: bool,
    clients: Vec<(u64, UnixStream)>,
    next_client: u64,
    changes: u64,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn change(&self, f: impl FnOnce(&mut Step)) {
        let mut g = self.lock();
        if g.closed {
            return;
        }
        let before = g.op.step.clone();
        f(&mut g.op.step);
        g.op.step.keys.truncate(MAX_KEYS);
        if g.op.step != before {
            g.op.updated = now_ms();
            g.seq += 1;
            self.changed.notify_all();
        }
    }

    fn drop_client(&self, id: u64) {
        self.lock().clients.retain(|(c, _)| *c != id);
    }
}

/// One line in one send on a non-blocking socket, or false. A client that cannot take a whole
/// line now has fallen behind. The socket itself is non-blocking because macOS blocks a unix
/// socket send that asks for `MSG_DONTWAIT`.
fn send(stream: &UnixStream, line: &[u8]) -> bool {
    #[cfg(target_os = "linux")]
    let flags = libc::MSG_NOSIGNAL;
    #[cfg(not(target_os = "linux"))]
    let flags = 0;
    // SAFETY: the pointer and length describe `line`, which outlives the call.
    let n = unsafe { libc::send(stream.as_raw_fd(), line.as_ptr().cast(), line.len(), flags) };
    n == line.len() as isize
}

/// Block until `stream`, a non-blocking socket, has something to read or has closed.
fn readable(stream: &UnixStream) -> bool {
    let mut fd = libc::pollfd { fd: stream.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    loop {
        // SAFETY: `fd` is one valid pollfd for the length of the call.
        match unsafe { libc::poll(&mut fd, 1, -1) } {
            n if n > 0 => return true,
            _ if std::io::Error::last_os_error().kind() == ErrorKind::Interrupted => {}
            _ => return false,
        }
    }
}

/// The listening end of one operation, for as long as it runs.
pub struct Publisher {
    shared: Arc<Shared>,
    path: PathBuf,
    broadcaster: Option<JoinHandle<()>>,
    acceptor: Option<JoinHandle<()>>,
}

impl Publisher {
    /// Listen for `op` in `dir`.
    pub fn start(dir: &Path, op: Operation) -> Result<Publisher> {
        ready(dir)?;
        sweep(dir);
        let path = dir.join(format!("{}.sock", op.id));
        let listener = UnixListener::bind(&path).with_context(|| format!("listening on {}", path.display()))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner { op, seq: 0, closed: false, clients: vec![], next_client: 0, changes: 0 }),
            changed: Condvar::new(),
        });
        let acceptor = {
            let shared = shared.clone();
            std::thread::Builder::new().name("activity-accept".into()).spawn(move || accept(&shared, &listener))?
        };
        let broadcaster = {
            let shared = shared.clone();
            std::thread::Builder::new().name("activity-send".into()).spawn(move || broadcast(&shared))?
        };
        Ok(Publisher { shared, path, broadcaster: Some(broadcaster), acceptor: Some(acceptor) })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn step(&self, f: impl FnOnce(&mut Step)) {
        self.shared.change(f);
    }

    /// Count `n` more changes in the result.
    pub fn changed(&self, n: u64) {
        self.shared.lock().changes += n;
    }

    /// Publish the end of the operation, close every client and stop listening.
    pub fn finish(mut self, error: Option<String>) {
        self.close(error);
    }

    fn close(&mut self, error: Option<String>) {
        {
            let mut g = self.shared.lock();
            if g.closed {
                return;
            }
            g.op.state = if error.is_some() { State::Failed } else { State::Succeeded };
            g.op.result = match error {
                Some(_) => None,
                None => Some(Outcome { changes: g.changes, summary: summary(g.op.kind, g.changes) }),
            };
            g.op.error = error;
            g.op.updated = now_ms();
            g.seq += 1;
            g.closed = true;
            self.shared.changed.notify_all();
        }
        if let Some(b) = self.broadcaster.take() {
            let _ = b.join();
        }
        // The acceptor is blocked in accept: one more connection lets it see `closed`.
        let woke = UnixStream::connect(&self.path).is_ok();
        let _ = std::fs::remove_file(&self.path);
        if let Some(a) = self.acceptor.take().filter(|_| woke) {
            let _ = a.join();
        }
    }
}

impl Drop for Publisher {
    fn drop(&mut self) {
        self.close(Some("fspec stopped before the operation finished".into()));
    }
}

fn accept(shared: &Arc<Shared>, listener: &UnixListener) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if stream.set_nonblocking(true).is_err() {
            continue;
        }
        let mut g = shared.lock();
        let snapshot = encode(Message::Snapshot { op: g.op.clone() });
        if g.closed {
            // Whoever connected before the listener went still gets the end of the operation.
            drop(g);
            send(&stream, &snapshot);
            if listener.set_nonblocking(true).is_ok() {
                for late in listener.incoming().map_while(Result::ok) {
                    send(&late, &snapshot);
                }
            }
            return;
        }
        let Ok(reader) = stream.try_clone() else { continue };
        if !send(&stream, &snapshot) {
            continue;
        }
        let id = g.next_client;
        g.next_client += 1;
        g.clients.push((id, stream));
        drop(g);
        let shared = shared.clone();
        let _ = std::thread::Builder::new().name("activity-read".into()).spawn(move || read(&shared, id, reader));
    }
}

/// A client's own lines: nothing from a shell, steps from a relay. The read also notices the
/// client going away while nothing is being sent.
fn read(shared: &Shared, id: u64, mut stream: UnixStream) {
    let (mut pending, mut chunk) = (Vec::new(), [0u8; 4096]);
    while readable(&stream) {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => pending.extend_from_slice(&chunk[..n]),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => continue,
            Err(_) => break,
        }
        while let Some(end) = pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = pending.drain(..=end).collect();
            let Ok(Line { v: VERSION, message }) = serde_json::from_slice::<Line>(&line) else { continue };
            match message {
                Message::Relay => {
                    shared.drop_client(id);
                    let _ = stream.shutdown(Shutdown::Write);
                }
                Message::Step { step } => shared.change(|s| *s = step),
                Message::Changed { changes } => shared.lock().changes += changes,
                Message::Snapshot { .. } | Message::Update { .. } => {}
            }
        }
    }
    shared.drop_client(id);
}

fn broadcast(shared: &Shared) {
    let mut sent = 0;
    loop {
        let mut g = shared.lock();
        while g.seq == sent && !g.closed {
            g = shared.changed.wait(g).unwrap_or_else(|e| e.into_inner());
        }
        if g.seq != sent {
            sent = g.seq;
            let line = encode(Message::Update { op: g.op.clone() });
            g.clients.retain(|(_, s)| {
                let kept = send(s, &line);
                if !kept {
                    let _ = s.shutdown(Shutdown::Both);
                }
                kept
            });
        }
        if g.closed && g.seq == sent {
            for (_, s) in g.clients.drain(..) {
                let _ = s.shutdown(Shutdown::Both);
            }
            return;
        }
        drop(g);
        std::thread::sleep(INTERVAL);
    }
}

/// The `sudo fspec` child's end: its steps, sent to the parent's socket.
struct Relay {
    stream: UnixStream,
    step: Step,
}

impl Relay {
    fn connect(path: &Path) -> Option<Relay> {
        let stream = UnixStream::connect(path).ok()?;
        stream.set_nonblocking(true).ok()?;
        send(&stream, &encode(Message::Relay)).then_some(Relay { stream, step: Step::default() })
    }

    /// False once the parent stopped reading.
    fn step(&mut self, f: impl FnOnce(&mut Step)) -> bool {
        let before = self.step.clone();
        f(&mut self.step);
        self.step.keys.truncate(MAX_KEYS);
        self.step == before || send(&self.stream, &encode(Message::Step { step: self.step.clone() }))
    }
}

enum Sink {
    Publisher(Publisher),
    Relay(Relay),
    /// A relay whose parent cannot be reached: the child publishes nothing of its own.
    Off,
}

/// This process's end of the stream. One operation per process.
static SINK: Mutex<Option<Sink>> = Mutex::new(None);

fn sink() -> MutexGuard<'static, Option<Sink>> {
    SINK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Run `body` as `op`, published in [`dir`] unless this process relays for a parent. A stream
/// that cannot start costs the operation nothing but a warning.
pub fn run<T>(op: Operation, body: impl FnOnce() -> Result<T>) -> Result<T> {
    let started = {
        let mut s = sink();
        match &*s {
            Some(_) => false,
            None => match Publisher::start(&dir(), op) {
                Ok(p) => {
                    *s = Some(Sink::Publisher(p));
                    true
                }
                Err(e) => {
                    eprintln!("fspec: no activity stream: {e:#}");
                    false
                }
            },
        }
    };
    let result = body();
    let taken = if started { sink().take() } else { None };
    if let Some(Sink::Publisher(p)) = taken {
        p.finish(result.as_ref().err().map(|e| format!("{e:#}")));
    }
    result
}

/// Send this process's steps to the parent operation listening at `path`.
pub fn relay(path: &Path) {
    *sink() = Some(Relay::connect(path).map_or(Sink::Off, Sink::Relay));
}

/// The socket a `sudo fspec` child relays to, while this process publishes an operation.
pub fn relay_path() -> Option<PathBuf> {
    match &*sink() {
        Some(Sink::Publisher(p)) => Some(p.path().to_path_buf()),
        _ => None,
    }
}

pub fn step(f: impl FnOnce(&mut Step)) {
    let mut s = sink();
    match &mut *s {
        Some(Sink::Publisher(p)) => p.step(f),
        Some(Sink::Relay(r)) => {
            if !r.step(f) {
                *s = Some(Sink::Off);
            }
        }
        Some(Sink::Off) | None => {}
    }
}

/// Enter `phase`, which has no progress or item yet.
pub fn phase(phase: Phase, scope: Option<Scope>) {
    step(|s| *s = Step { phase, scope, ..Step::default() });
}

pub fn progress(done: u64, total: u64, unit: Unit, item: Option<String>, keys: Vec<String>) {
    step(|s| {
        s.progress = Some(Progress { done, total, unit });
        s.item = item;
        s.keys = keys;
    });
}

/// Count `n` more changes in the operation's result.
pub fn changed(n: u64) {
    let mut s = sink();
    match &mut *s {
        Some(Sink::Publisher(p)) => p.changed(n),
        Some(Sink::Relay(r)) => {
            if !send(&r.stream, &encode(Message::Changed { changes: n })) {
                *s = Some(Sink::Off);
            }
        }
        Some(Sink::Off) | None => {}
    }
}

fn summary(kind: Kind, n: u64) -> String {
    match (kind, n) {
        (Kind::Update, 0) => "the lock is unchanged".into(),
        (Kind::Update, 1) => "1 lock entry changed".into(),
        (Kind::Update, n) => format!("{n} lock entries changed"),
        (_, 0) => "no changes".into(),
        (_, 1) => "1 change".into(),
        (_, n) => format!("{n} changes"),
    }
}

/// `first, second and N more`, for an item that is a batch.
pub fn batch_label(names: &[String]) -> String {
    let unique: Vec<&String> = names.iter().collect::<BTreeSet<_>>().into_iter().collect();
    match unique.as_slice() {
        [] => String::new(),
        [one] => one.to_string(),
        [a, b] => format!("{a} and {b}"),
        [a, b, rest @ ..] => format!("{a}, {b} and {} more", rest.len()),
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or_default()
}

#[cfg(test)]
mod tests;
