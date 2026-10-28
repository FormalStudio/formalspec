use std::fmt;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Mutex;

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
        let out = self.run(cmd)?;
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
