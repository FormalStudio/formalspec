use std::fmt;
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// An external command. Every package manager, init system, bootc and ssh call goes through a
/// [`Runner`] so tests can record them instead of touching the machine.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Cmd {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdin: Option<String>,
    /// A file streamed to stdin, for archives too large to hold as a `String`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdin_file: Option<std::path::PathBuf>,
    /// Hand the terminal to the command (`ssh -t` for a sudo prompt); output is not captured.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub interactive: bool,
}

impl Cmd {
    pub fn new(program: impl Into<String>) -> Cmd {
        Cmd { program: program.into(), args: vec![], env: vec![], stdin: None, stdin_file: None, interactive: false }
    }

    pub fn arg(mut self, a: impl Into<String>) -> Cmd {
        self.args.push(a.into());
        self
    }

    pub fn args<I: IntoIterator<Item = S>, S: Into<String>>(mut self, args: I) -> Cmd {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn env(mut self, k: impl Into<String>, v: impl Into<String>) -> Cmd {
        self.env.push((k.into(), v.into()));
        self
    }

    pub fn stdin(mut self, input: impl Into<String>) -> Cmd {
        self.stdin = Some(input.into());
        self
    }

    pub fn stdin_file(mut self, path: impl Into<std::path::PathBuf>) -> Cmd {
        self.stdin_file = Some(path.into());
        self
    }

    pub fn interactive(mut self) -> Cmd {
        self.interactive = true;
        self
    }
}

impl fmt::Display for Cmd {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.program)?;
        for a in &self.args {
            if a.contains(char::is_whitespace) { write!(f, " '{a}'")? } else { write!(f, " {a}")? }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Output {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

pub trait Runner: Sync {
    fn run(&self, cmd: &Cmd) -> Result<Output>;

    /// Run and fail on a non-zero exit, with stderr in the error, or the end of stdout for tools
    /// like moss that report errors there.
    fn check(&self, cmd: &Cmd) -> Result<Output> {
        self.check_ok(cmd, &[0])
    }

    /// `check`, taking any of the `ok` exit codes as success.
    fn check_ok(&self, cmd: &Cmd, ok: &[i32]) -> Result<Output> {
        checked(cmd, self.run(cmd)?, ok)
    }

    /// `run`, handing `watch` each output line as it arrives (`Some`, split on `\n` and `\r`, so
    /// a progress bar redrawn in place is a line per redraw) and `None` about once a second while
    /// the command is quiet. A runner that cannot stream hands over the lines when it is done.
    fn run_watched(&self, cmd: &Cmd, watch: &mut dyn FnMut(Option<&str>)) -> Result<Output> {
        let out = self.run(cmd)?;
        for line in out.stdout.split(['\n', '\r']).chain(out.stderr.split(['\n', '\r'])) {
            watch(Some(line));
        }
        Ok(out)
    }

    /// `check` through [`Runner::run_watched`].
    fn check_watched(&self, cmd: &Cmd, watch: &mut dyn FnMut(Option<&str>)) -> Result<Output> {
        checked(cmd, self.run_watched(cmd, watch)?, &[0])
    }
}

fn checked(cmd: &Cmd, out: Output, ok: &[i32]) -> Result<Output> {
    if !ok.contains(&out.status) {
        let why = match out.stderr.trim() {
            "" => {
                let lines: Vec<&str> = out.stdout.trim().lines().collect();
                lines[lines.len().saturating_sub(5)..].join("\n")
            }
            e => e.to_string(),
        };
        bail!("`{cmd}` exited {}: {why}", out.status);
    }
    Ok(out)
}

pub struct LiveRunner;

impl Runner for LiveRunner {
    fn run(&self, cmd: &Cmd) -> Result<Output> {
        let mut c = Command::new(&cmd.program);
        c.args(&cmd.args).envs(cmd.env.iter().map(|(k, v)| (k, v)));
        if cmd.interactive {
            let status = c.status().with_context(|| format!("running `{cmd}`"))?;
            return Ok(Output { status: status.code().unwrap_or(-1), ..Output::default() });
        }
        if let Some(path) = &cmd.stdin_file {
            let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
            c.stdin(file).stdout(Stdio::piped()).stderr(Stdio::piped());
            let out = c.output().with_context(|| format!("running `{cmd}`"))?;
            return Ok(Output {
                status: out.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&out.stdout).into(),
                stderr: String::from_utf8_lossy(&out.stderr).into(),
            });
        }
        c.stdin(if cmd.stdin.is_some() { Stdio::piped() } else { Stdio::null() });
        c.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = c.spawn().with_context(|| format!("running `{cmd}`"))?;
        // Feed stdin from its own thread: a child that fills its stdout or stderr pipe before it
        // has read all of stdin (tar -T - warning about thousands of paths) would otherwise
        // block on that write while we block on ours.
        let writer = match (&cmd.stdin, child.stdin.take()) {
            (Some(input), Some(mut stdin)) => {
                let input = input.clone();
                Some(std::thread::spawn(move || stdin.write_all(input.as_bytes())))
            }
            _ => None,
        };
        let out = child.wait_with_output()?;
        if let Some(w) = writer {
            // A child that exits without reading everything closes the pipe; its exit status
            // already says what went wrong.
            let _ = w.join();
        }
        Ok(Output {
            status: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into(),
            stderr: String::from_utf8_lossy(&out.stderr).into(),
        })
    }

    fn run_watched(&self, cmd: &Cmd, watch: &mut dyn FnMut(Option<&str>)) -> Result<Output> {
        if cmd.interactive || cmd.stdin.is_some() || cmd.stdin_file.is_some() {
            let out = self.run(cmd)?;
            for line in out.stdout.split(['\n', '\r']).chain(out.stderr.split(['\n', '\r'])) {
                watch(Some(line));
            }
            return Ok(out);
        }
        let mut child = Command::new(&cmd.program)
            .args(&cmd.args)
            .envs(cmd.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("running `{cmd}`"))?;
        let (tx, rx) = std::sync::mpsc::channel::<(usize, Vec<u8>)>();
        let pipes: [Box<dyn Read + Send>; 2] =
            [Box::new(child.stdout.take().context("stdout")?), Box::new(child.stderr.take().context("stderr")?)];
        let readers: Vec<_> = pipes
            .into_iter()
            .enumerate()
            .map(|(i, mut pipe)| {
                let tx = tx.clone();
                std::thread::spawn(move || {
                    let mut buf = [0u8; 8192];
                    while let Ok(n) = pipe.read(&mut buf) {
                        if n == 0 || tx.send((i, buf[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                })
            })
            .collect();
        drop(tx);
        let mut streams = [Lines::default(), Lines::default()];
        loop {
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok((i, chunk)) => streams[i].push(&chunk, watch),
                Err(RecvTimeoutError::Timeout) => watch(None),
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        for r in readers {
            let _ = r.join();
        }
        let status = child.wait()?.code().unwrap_or(-1);
        let [stdout, stderr] = streams.map(|s| s.finish(watch));
        Ok(Output { status, stdout, stderr })
    }
}

/// One pipe's bytes cut into lines for a watch. What it keeps is what a terminal would end up
/// showing: a segment ended by a lone `\r` is redrawn over, so a progress bar redrawn ten
/// thousand times does not end up ten thousand times in an error message.
#[derive(Default)]
struct Lines {
    pending: Vec<u8>,
    kept: Vec<u8>,
}

impl Lines {
    fn push(&mut self, chunk: &[u8], watch: &mut dyn FnMut(Option<&str>)) {
        self.pending.extend_from_slice(chunk);
        let mut start = 0;
        while let Some(at) = self.pending[start..].iter().position(|b| matches!(b, b'\n' | b'\r')) {
            let end = start + at;
            let crlf = match self.pending[end] {
                b'\r' => match self.pending.get(end + 1) {
                    // Whether this `\r` starts a `\r\n` is up to the next chunk.
                    None => break,
                    Some(b'\n') => true,
                    Some(_) => false,
                },
                _ => true,
            };
            let line = &self.pending[start..end];
            watch(Some(&String::from_utf8_lossy(line)));
            if crlf {
                self.kept.extend_from_slice(line);
                self.kept.push(b'\n');
            }
            start = end + if crlf && self.pending[end] == b'\r' { 2 } else { 1 };
        }
        self.pending.drain(..start);
    }

    fn finish(mut self, watch: &mut dyn FnMut(Option<&str>)) -> String {
        if self.pending.last() == Some(&b'\r') {
            self.pending.pop();
            watch(Some(&String::from_utf8_lossy(&self.pending)));
        } else if !self.pending.is_empty() {
            watch(Some(&String::from_utf8_lossy(&self.pending)));
            self.kept.extend_from_slice(&self.pending);
        }
        String::from_utf8_lossy(&self.kept).into()
    }
}

/// Appends each command to a log file instead of running it, answering `status 0`. The CLI uses
/// it whenever `--root` is not `/`, so a rooted apply never runs a package manager or init system.
pub struct LogRunner {
    pub log: std::path::PathBuf,
}

impl Runner for LogRunner {
    fn run(&self, cmd: &Cmd) -> Result<Output> {
        if let Some(dir) = self.log.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&self.log)?;
        // One write per line: queries run from several threads, and an append of one buffer
        // does not interleave with another's.
        f.write_all(format!("{cmd}\n").as_bytes())?;
        Ok(Output::default())
    }
}

/// Records every command and answers from canned responses, `status 0` and empty output by default.
#[derive(Default)]
pub struct RecordingRunner {
    pub calls: Mutex<Vec<Cmd>>,
    /// Matched against the start of `Cmd`'s display form, first match wins.
    pub responses: Mutex<Vec<(String, Output)>>,
}

impl RecordingRunner {
    pub fn respond(&self, prefix: &str, out: Output) {
        self.responses.lock().unwrap().push((prefix.into(), out));
    }

    /// Every recorded command, display form.
    pub fn lines(&self) -> Vec<String> {
        self.calls.lock().unwrap().iter().map(ToString::to_string).collect()
    }
}

impl Runner for RecordingRunner {
    fn run(&self, cmd: &Cmd) -> Result<Output> {
        self.calls.lock().unwrap().push(cmd.clone());
        let line = cmd.to_string();
        let responses = self.responses.lock().unwrap();
        Ok(responses.iter().find(|(p, _)| line.starts_with(p.as_str())).map(|(_, o)| o.clone()).unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_watched_run_streams_every_redraw_and_keeps_what_a_terminal_shows() {
        let cmd = Cmd::new("sh").args(["-c", r#"printf 'bar 1\rbar 2\rdone\r\nnext\n'; printf 'oops' >&2; exit 3"#]);
        let mut seen = Vec::new();
        let out = LiveRunner.run_watched(&cmd, &mut |l| seen.extend(l.map(str::to_string))).unwrap();
        assert_eq!(out.status, 3);
        assert_eq!(out.stdout, "done\nnext\n");
        assert_eq!(out.stderr, "oops");
        for want in ["bar 1", "bar 2", "done", "next", "oops"] {
            assert!(seen.iter().any(|l| l == want), "{want} in {seen:?}");
        }
    }
}
