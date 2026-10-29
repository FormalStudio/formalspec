use std::path::PathBuf;

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use super::{Flow, Hints, Shared, hints};
use crate::keys::{Key, Kind, Source};
use crate::model::Secret;
use crate::ui::anim;
use crate::ui::theme::fg;
use crate::ui::widgets::controls::{Kind as Button, button, segmented, toggle};
use crate::ui::widgets::input::{FIELD_ROWS, Field, TextInput, draw_field};
use crate::ui::widgets::{Canvas, Hit, rect_in, wrap};

const METHOD: usize = 0;
const INPUT: usize = 1;
const PASS: usize = 2;
const ADD: usize = 3;
const GENERATE: usize = 4;
const GO: usize = 5;

const PASTE: usize = 0;
const FILE: usize = 1;
const SSH: usize = 2;

/// A key the user brought, and which of the spec's secrets it opens.
pub struct Added {
    pub key: Key,
    pub opens: Option<Vec<(String, bool)>>,
}

pub struct KeysScreen {
    method: usize,
    paste: TextInput,
    file: TextInput,
    from: TextInput,
    password: TextInput,
    focus: usize,
    pub fetching: Option<String>,
    error: Option<String>,
    pub added: Vec<Added>,
    generate: bool,
    /// Made the first time the switch goes on, so its public key stays the same after.
    generated: Option<Key>,
    /// The spec's secrets; `None` when it has no `secrets.star`.
    secrets: Option<Vec<(String, bool)>>,
}

impl Default for KeysScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl KeysScreen {
    pub fn new() -> Self {
        KeysScreen {
            method: PASTE,
            paste: TextInput::new(true),
            file: TextInput::default(),
            from: TextInput::default(),
            password: TextInput::new(true),
            focus: INPUT,
            fetching: None,
            error: None,
            added: Vec::new(),
            generate: false,
            generated: None,
            secrets: None,
        }
    }

    /// Called whenever the screen is entered, since another spec may have been cloned.
    pub fn enter(&mut self, sh: &Shared) {
        self.secrets = sh.be.secrets(&sh.checkout, None).ok().flatten();
        for a in &mut self.added {
            a.opens = sh.be.secrets(&sh.checkout, Some(&a.key)).ok().flatten();
        }
    }

    /// The keys to install: the ones brought, then the new one.
    pub fn value(&self) -> Vec<Key> {
        let brought = self.added.iter().map(|a| a.key.clone());
        brought.chain(self.generated.clone().filter(|_| self.generate)).collect()
    }

    pub fn captures_text(&self) -> bool {
        matches!(self.focus, INPUT | PASS)
    }

    fn input(&mut self) -> &mut TextInput {
        match self.method {
            PASTE => &mut self.paste,
            FILE => &mut self.file,
            _ => &mut self.from,
        }
    }

    /// Where to read the key from, or `None` while its field is empty.
    pub fn request(&self) -> Option<Source> {
        let text = |t: &TextInput| Some(t.value.trim().to_string()).filter(|v| !v.is_empty());
        Some(match self.method {
            PASTE => Source::Paste(Secret(text(&self.paste)?)),
            FILE => Source::File(PathBuf::from(text(&self.file)?)),
            _ => Source::Ssh {
                from: text(&self.from)?,
                password: Some(Secret(self.password.value.clone())).filter(|p| !p.0.is_empty()),
            },
        })
    }

    pub fn fetched(&mut self, result: Result<Key, String>, sh: &Shared) {
        self.fetching = None;
        let key = match result {
            Ok(k) => k,
            Err(e) => {
                let last = e.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
                self.error = Some(format!("Unable to read the key: {last}"));
                return;
            }
        };
        if self.added.iter().any(|a| a.key.private == key.private) {
            self.error = Some("That key is already added.".into());
            return;
        }
        if key.kind != Kind::Age && self.added.iter().any(|a| a.key.kind == key.kind) {
            self.error = Some(format!("An {} is already added; the new system keeps one of each.", key.label()));
            return;
        }
        self.error = None;
        let opens = sh.be.secrets(&sh.checkout, Some(&key)).ok().flatten();
        self.added.push(Added { key, opens });
        self.input().set("");
        self.password.set("");
        self.focus = GO;
    }

    fn stops(&self) -> Vec<usize> {
        if self.method == SSH {
            vec![METHOD, INPUT, PASS, ADD, GENERATE, GO]
        } else {
            vec![METHOD, INPUT, ADD, GENERATE, GO]
        }
    }

    fn shift(&mut self, d: isize) {
        let s = self.stops();
        let p = s.iter().position(|&f| f == self.focus).unwrap_or(0) as isize;
        self.focus = s[(p + d).rem_euclid(s.len() as isize) as usize];
    }

    fn toggle_generate(&mut self) {
        self.generate = !self.generate;
        if self.generate && self.generated.is_none() {
            self.generated = Some(Key::generate());
        }
    }

    fn add(&mut self) -> Flow {
        match self.request() {
            Some(source) => {
                self.error = None;
                self.fetching = Some(match &source {
                    Source::Paste(_) => "Reading the key".into(),
                    Source::File(p) => format!("Reading {}", p.display()),
                    Source::Ssh { from, .. } => format!("Reading {from}"),
                });
                Flow::FetchKey
            }
            None => {
                self.focus = INPUT;
                self.error = Some("Enter a key first, or press Continue to go on without one.".into());
                Flow::Stay
            }
        }
    }

    pub fn key(&mut self, k: &KeyEvent, _sh: &mut Shared) -> Flow {
        if self.fetching.is_some() {
            return Flow::Stay;
        }
        match k.code {
            KeyCode::Tab | KeyCode::Down => self.shift(1),
            KeyCode::BackTab | KeyCode::Up => self.shift(-1),
            KeyCode::Esc => return Flow::Back,
            KeyCode::Left if self.focus == METHOD => self.method = self.method.saturating_sub(1),
            KeyCode::Right if self.focus == METHOD => self.method = (self.method + 1).min(SSH),
            KeyCode::Char(' ') if self.focus == GENERATE => self.toggle_generate(),
            KeyCode::Enter => match self.focus {
                // An empty field goes on, so skipping the step is one Enter.
                METHOD | INPUT | PASS if self.request().is_none() => return Flow::Next,
                METHOD | INPUT | PASS | ADD => return self.add(),
                GENERATE => self.toggle_generate(),
                _ => return Flow::Next,
            },
            _ => match self.focus {
                INPUT => {
                    self.input().key(k);
                }
                PASS => {
                    self.password.key(k);
                }
                _ => {}
            },
        }
        Flow::Stay
    }

    pub fn click(&mut self, h: Hit) -> Flow {
        if self.fetching.is_some() {
            return Flow::Stay;
        }
        match h.w as usize {
            METHOD => {
                self.focus = METHOD;
                self.method = h.i.min(SSH);
            }
            INPUT => self.focus = INPUT,
            PASS => self.focus = PASS,
            ADD => {
                self.focus = ADD;
                return self.add();
            }
            GENERATE => {
                self.focus = GENERATE;
                self.toggle_generate();
            }
            GO => return Flow::Next,
            _ => {}
        }
        Flow::Stay
    }

    pub fn hints(&self) -> Hints {
        hints(&[("Tab", "Next field"), ("←→", "Where from"), ("Enter", "Add or continue"), ("Esc", "Back")])
    }

    pub fn render(&mut self, c: &mut Canvas, a: Rect, _sh: &Shared) {
        let th = *c.th;
        let w = a.width.min(66);
        let bold = Style::new().fg(th.ink).add_modifier(Modifier::BOLD);
        c.text(a, 0, 0, "Bring a key for your secrets", bold);
        c.text_ell(a, 0, 1, "Optional. Without one, secrets are left out; the rest installs.", fg(th.muted));
        let status = match &self.secrets {
            None => "Your spec has no secrets.star, so nothing needs a key yet.".to_string(),
            Some(s) if s.len() == 1 => "Your spec has 1 secret in secrets.star.".to_string(),
            Some(s) => format!("Your spec has {} secrets in secrets.star.", s.len()),
        };
        c.text_ell(a, 0, 2, &status, fg(th.muted));

        let seg = rect_in(a, 0, 4, w, 2);
        c.text(seg, 0, 0, "Get a key from", if self.focus == METHOD { fg(th.accent) } else { fg(th.muted) });
        let row = Rect { y: seg.y + 1, height: 1, ..seg };
        segmented(c, row, 0, &["Paste", "File", "Another machine"], self.method, self.focus == METHOD, METHOD as u8);
        let mut y = 7;
        let (label, input, placeholder, hint) = match self.method {
            PASTE => ("Age key", &self.paste, "AGE-SECRET-KEY-1…", None),
            FILE => {
                ("Key file", &self.file, "/media/usb/keys.txt", Some("An age identity file or an SSH private key."))
            }
            _ => ("From", &self.from, "you@laptop:~/.config/age/keys.txt", None),
        };
        let field = |input, label, placeholder, hint, focused, w_id: usize| Field {
            label,
            input,
            placeholder,
            focused,
            error: None,
            hint,
            w: w_id as u8,
            i: 0,
        };
        draw_field(
            c,
            rect_in(a, 0, y, w, FIELD_ROWS),
            field(input, label, placeholder, hint, self.focus == INPUT, INPUT),
        );
        y += FIELD_ROWS;
        if self.method == SSH {
            let hint = Some("Leave it empty to sign in with an SSH key.");
            draw_field(
                c,
                rect_in(a, 0, y, w, FIELD_ROWS),
                field(&self.password, "Password", "", hint, self.focus == PASS, PASS),
            );
            y += FIELD_ROWS;
        }
        let r = rect_in(a, 0, y, a.width, 1);
        if let Some(what) = &self.fetching {
            c.text(r, 0, 0, anim::spinner(c.epoch, c.now), fg(th.accent));
            c.text_ell(r, 2, 0, what, fg(th.ink));
        } else {
            button(c, r, 0, "Add key", Button::Secondary, self.focus == ADD, Hit { w: ADD as u8, i: 0 });
        }
        y += 1;
        if let Some(e) = &self.error {
            for l in wrap(e, w as usize) {
                c.text(a, 0, y, &l, fg(th.red));
                y += 1;
            }
        }
        y += 1;
        for added in &self.added {
            let (mark, color, what) = match &added.opens {
                None => ("✓", th.green, String::new()),
                Some(all) => {
                    let n = all.iter().filter(|(_, o)| *o).count();
                    match (n, all.len()) {
                        (_, 0) => ("✓", th.green, String::new()),
                        (0, total) => ("!", th.amber, format!(" opens none of the {total} secrets")),
                        (n, total) if n == total => ("✓", th.green, format!(" opens all {total} secrets")),
                        (n, total) => ("✓", th.green, format!(" opens {n} of {total} secrets")),
                    }
                }
            };
            c.text(a, 0, y, mark, fg(color));
            let x = 2 + c.text(a, 2, y, &added.key.label(), fg(th.ink));
            c.text_ell(Rect { width: w, ..a }, x, y, &what, fg(if mark == "!" { th.amber } else { th.muted }));
            y += 1;
        }
        if !self.added.is_empty() {
            y += 1;
        }
        let on = self.generate;
        toggle(
            c,
            rect_in(a, 0, y, w, 1),
            on,
            "Also make a new age key for this machine",
            self.focus == GENERATE,
            true,
            Hit { w: GENERATE as u8, i: 0 },
        );
        y += 1;
        if let Some(k) = self.generated.as_ref().filter(|_| on) {
            c.text_ell(a, 7, y, &k.public[0], fg(th.faint));
            y += 1;
        }
        y += 1;
        let label = if self.value().is_empty() { "Continue without a key" } else { "Continue" };
        button(c, rect_in(a, 0, y, a.width, 1), 0, label, Button::Primary, self.focus == GO, Hit { w: GO as u8, i: 0 });
    }
}
