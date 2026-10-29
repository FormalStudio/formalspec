use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::time::Instant;

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use super::{Exit, Flow, Hints, Step, hints};
use crate::model::Progress;
use crate::ui::anim;
use crate::ui::theme::fg;
use crate::ui::widgets::controls::{Kind, button};
use crate::ui::widgets::{Canvas, Hit, rect_in, wrap};

const LOG_CAP: usize = 400;

pub enum Outcome {
    Running,
    Done,
    Failed { stage: String, error: String, log: PathBuf },
}

pub struct InstallScreen {
    rx: Option<Receiver<Progress>>,
    started: Instant,
    finished: Option<Instant>,
    stages: Vec<String>,
    total: usize,
    log: VecDeque<String>,
    pub outcome: Outcome,
    focus: usize,
    name: String,
    /// What to do so the new key opens the spec's secrets, when the installer made one.
    notes: Vec<String>,
    /// Secrets the first apply left out, since no key in the new system opens them.
    skipped: Vec<String>,
}

impl Default for InstallScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl InstallScreen {
    pub fn new() -> Self {
        InstallScreen {
            rx: None,
            started: Instant::now(),
            finished: None,
            stages: Vec::new(),
            total: 0,
            log: VecDeque::new(),
            outcome: Outcome::Running,
            focus: 0,
            name: String::new(),
            notes: Vec::new(),
            skipped: Vec::new(),
        }
    }

    pub fn start(&mut self, rx: Receiver<Progress>, now: Instant, name: &str, notes: Vec<String>) {
        *self = InstallScreen::new();
        self.rx = Some(rx);
        self.started = now;
        self.name = name.to_string();
        self.notes = notes;
    }

    pub fn running(&self) -> bool {
        self.rx.is_some() && matches!(self.outcome, Outcome::Running)
    }

    pub fn captures_text(&self) -> bool {
        false
    }

    pub fn poll(&mut self, now: Instant) {
        let Some(rx) = &self.rx else { return };
        while let Ok(p) = rx.try_recv() {
            match p {
                Progress::Stage { total, title, .. } => {
                    self.total = total;
                    self.stages.push(title);
                }
                Progress::Output(l) => {
                    self.skipped.extend(crate::keys::skipped_secret(&l).map(str::to_string));
                    if self.log.len() == LOG_CAP {
                        self.log.pop_front();
                    }
                    self.log.push_back(l);
                }
                Progress::Done => {
                    self.outcome = Outcome::Done;
                    self.finished = Some(now);
                }
                Progress::Failed { stage, error, log } => {
                    self.outcome = Outcome::Failed { stage, error, log };
                    self.finished = Some(now);
                }
            }
        }
    }

    fn button_count(&self) -> usize {
        2
    }

    fn activate(&mut self, i: usize) -> Flow {
        match (&self.outcome, i) {
            (Outcome::Done, 0) => Flow::Exit(Exit::Restart),
            (Outcome::Failed { .. }, 0) => Flow::Jump(Step::Review),
            (Outcome::Running, _) => Flow::Stay,
            _ => Flow::Exit(Exit::Shell),
        }
    }

    pub fn key(&mut self, k: &KeyEvent) -> Flow {
        if self.running() {
            return Flow::Stay;
        }
        match k.code {
            KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                self.focus = (self.focus + 1) % self.button_count()
            }
            KeyCode::Enter => return self.activate(self.focus),
            _ => {}
        }
        Flow::Stay
    }

    pub fn click(&mut self, h: Hit) -> Flow {
        if h.w == 0 && !self.running() {
            self.focus = h.i;
            return self.activate(h.i);
        }
        Flow::Stay
    }

    pub fn hints(&self) -> Hints {
        if self.running() {
            hints(&[("", "Installing. This cannot be interrupted.")])
        } else {
            hints(&[("←→", "Choose"), ("Enter", "Select")])
        }
    }

    fn elapsed(&self, now: Instant) -> String {
        let end = self.finished.unwrap_or(now);
        let s = end.saturating_duration_since(self.started).as_secs();
        format!("{:02}:{:02}", s / 60, s % 60)
    }

    pub fn render(&mut self, c: &mut Canvas, a: Rect) {
        let th = *c.th;
        let side = a.width >= 90;
        let left_w = if side { 56.min(a.width / 2) } else { a.width };
        let left = rect_in(a, 0, 0, left_w, a.height);
        let bold = Style::new().fg(th.ink).add_modifier(Modifier::BOLD);
        let mut y = 0;
        let failed_stage = match &self.outcome {
            Outcome::Failed { stage, .. } => Some(stage.clone()),
            _ => None,
        };
        match &self.outcome {
            Outcome::Running => {
                c.text(left, 0, y, &format!("Installing {}", self.name), bold);
                c.text(left, 0, y + 1, "Keep the machine powered on.", fg(th.muted));
                y += 3;
            }
            Outcome::Done => {
                c.text(left, 0, y, "✓", Style::new().fg(th.green).add_modifier(Modifier::BOLD));
                c.text(left, 2, y, &format!("{} is installed.", self.name), bold);
                c.text(left, 0, y + 1, "Remove the installer USB, then restart.", fg(th.muted));
                let r = rect_in(left, 0, y + 3, left.width, 1);
                let w = button(c, r, 0, "Restart now", Kind::Primary, self.focus == 0, Hit { w: 0, i: 0 });
                button(c, r, w + 2, "Open a shell", Kind::Secondary, self.focus == 1, Hit { w: 0, i: 1 });
                y += 5;
                if !self.skipped.is_empty() {
                    let n = self.skipped.len();
                    let what = if n == 1 { "1 secret was".into() } else { format!("{n} secrets were") };
                    let text = format!(
                        "{what} left out, since no key in the new system opens it: {}. Put a key at \
                         ~/.config/fspec/age.key, then run fspec apply.",
                        self.skipped.join(", ")
                    );
                    for l in wrap(&text, a.width as usize) {
                        c.text(a, 0, y, &l, fg(th.amber));
                        y += 1;
                    }
                    y += 1;
                }
                for l in &self.notes {
                    c.text_ell(a, 0, y, l, fg(th.ink));
                    y += 1;
                }
                if !self.notes.is_empty() {
                    y += 1;
                }
            }
            Outcome::Failed { stage, error, log } => {
                c.text(
                    left,
                    0,
                    y,
                    &format!("The install stopped during {}.", lower_first(stage)),
                    Style::new().fg(th.red).add_modifier(Modifier::BOLD),
                );
                y += 1;
                for l in wrap(error, left.width as usize).iter().take(3) {
                    c.text(left, 0, y, l, fg(th.ink));
                    y += 1;
                }
                c.text_ell(left, 0, y, &format!("The full log is at {}.", log.display()), fg(th.muted));
                y += 2;
                let r = rect_in(left, 0, y, left.width, 1);
                let w = button(c, r, 0, "Try again", Kind::Primary, self.focus == 0, Hit { w: 0, i: 0 });
                button(c, r, w + 2, "Open a shell", Kind::Secondary, self.focus == 1, Hit { w: 0, i: 1 });
                y += 2;
            }
        }
        // Stage list.
        let reserved = 3;
        let room = left.height.saturating_sub(y + reserved) as usize;
        let room = if side { room } else { room.min(6) };
        let n = self.stages.len();
        let more = self.total.saturating_sub(n);
        let rows = n + usize::from(more > 0 && matches!(self.outcome, Outcome::Running));
        let first = rows.saturating_sub(room.max(1));
        let mut ly = y;
        for i in first..rows {
            if i == n {
                c.text(left, 2, ly, &format!("{more} more steps"), fg(th.faint));
            } else {
                let title = &self.stages[i];
                let last = i + 1 == n;
                if last && matches!(self.outcome, Outcome::Running) {
                    c.text(left, 0, ly, anim::spinner(c.epoch, c.now), fg(th.accent));
                    c.text_ell(left, 2, ly, title, bold);
                } else if last && failed_stage.is_some() {
                    c.text(left, 0, ly, "✗", fg(th.red));
                    c.text_ell(left, 2, ly, title, fg(th.ink));
                } else {
                    c.text(left, 0, ly, "✓", fg(th.green));
                    c.text_ell(left, 2, ly, title, fg(th.muted));
                }
            }
            ly += 1;
        }
        y = ly + 1;
        let frac = match self.outcome {
            Outcome::Done => 1.0,
            _ if self.total == 0 => 0.0,
            _ => n.saturating_sub(1) as f32 / self.total as f32,
        };
        let filled = (frac * left.width as f32).round() as u16;
        for x in 0..left.width {
            c.text(left, x, y, "━", fg(if x < filled { th.accent } else { th.line }));
        }
        let step = if self.total == 0 { String::new() } else { format!("Step {} of {}", n.max(1), self.total) };
        c.text(left, 0, y + 1, &step, fg(th.muted));
        c.text_right(left, y + 1, &self.elapsed(c.now), fg(th.muted));
        let bottom = y + 2;
        // Log pane.
        let (log, ly0) = if side {
            (rect_in(a, left_w + 3, 0, a.width.saturating_sub(left_w + 3), a.height), 0)
        } else {
            (rect_in(a, 0, bottom + 1, a.width, a.height.saturating_sub(bottom + 1)), 0)
        };
        // The notes on a finished install run the full width, over where the log would be.
        let noted = matches!(self.outcome, Outcome::Done) && !(self.notes.is_empty() && self.skipped.is_empty());
        if log.height < 2 || noted {
            return;
        }
        c.text(log, 0, ly0, "Log", Style::new().fg(th.muted).add_modifier(Modifier::BOLD));
        let lines = log.height.saturating_sub(2) as usize;
        let skip = self.log.len().saturating_sub(lines);
        for (i, l) in self.log.iter().skip(skip).enumerate() {
            c.text_ell(log, 0, ly0 + 2 + i as u16, l, fg(th.muted));
        }
    }
}

fn lower_first(s: &str) -> String {
    let mut cs = s.chars();
    cs.next().map(|c| c.to_lowercase().chain(cs).collect()).unwrap_or_default()
}
