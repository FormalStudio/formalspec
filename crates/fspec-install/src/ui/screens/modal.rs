use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use crate::ui::theme::fg;
use crate::ui::widgets::controls::{Kind, button};
use crate::ui::widgets::input::{FIELD_ROWS, Field, TextInput, draw_field};
use crate::ui::widgets::{Canvas, Hit, W_MODAL, rect_in, wrap};

pub enum Modal {
    Quit {
        focus: usize,
    },
    /// Confirms the install by its device path. `beside` is what happens to Windows when the
    /// install keeps it; without it the disk is erased.
    Erase {
        input: TextInput,
        focus: usize,
        device: String,
        model: String,
        beside: Option<String>,
    },
    Help,
}

pub enum Out {
    Stay,
    Close,
    Quit,
    Install,
}

const HELP: &[(&str, &str)] = &[
    ("↑↓  j k", "Move in a list"),
    ("PgUp PgDn", "Move by a page"),
    ("Home End", "First or last row"),
    ("Tab  Shift+Tab", "Move between fields"),
    ("Space", "Toggle a switch"),
    ("←→", "Change a choice or a number"),
    ("Enter", "Continue"),
    ("Esc", "Back"),
    ("s", "Show or hide the spec"),
    ("Ctrl+C", "Quit"),
];

impl Modal {
    pub fn erase(device: &str, model: &str, beside: Option<String>) -> Modal {
        Modal::Erase { input: TextInput::default(), focus: 0, device: device.into(), model: model.into(), beside }
    }

    pub fn quit() -> Modal {
        Modal::Quit { focus: 1 }
    }

    pub fn key(&mut self, k: &KeyEvent) -> Out {
        match self {
            Modal::Quit { focus } => match k.code {
                KeyCode::Esc => Out::Close,
                KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                    *focus = 1 - *focus;
                    Out::Stay
                }
                KeyCode::Enter => {
                    if *focus == 0 {
                        Out::Quit
                    } else {
                        Out::Close
                    }
                }
                _ => Out::Stay,
            },
            Modal::Erase { input, focus, device, .. } => {
                let ok = input.value == *device;
                match k.code {
                    KeyCode::Esc => return Out::Close,
                    KeyCode::Tab => *focus = next(*focus, ok, 1),
                    KeyCode::BackTab => *focus = next(*focus, ok, -1),
                    KeyCode::Right if *focus > 0 => *focus = next(*focus, ok, 1),
                    KeyCode::Left if *focus > 0 => *focus = next(*focus, ok, -1),
                    KeyCode::Enter => {
                        return match *focus {
                            2 => Out::Close,
                            _ if ok => Out::Install,
                            _ => Out::Stay,
                        };
                    }
                    _ if *focus == 0 => {
                        input.key(k);
                    }
                    _ => {}
                }
                Out::Stay
            }
            Modal::Help => match k.code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('?' | 'q') => Out::Close,
                _ => Out::Stay,
            },
        }
    }

    pub fn click(&mut self, h: Hit) -> Out {
        if h.w != W_MODAL {
            return Out::Stay;
        }
        match self {
            Modal::Quit { .. } => {
                if h.i == 0 {
                    Out::Quit
                } else {
                    Out::Close
                }
            }
            Modal::Erase { input, focus, device, .. } => match h.i {
                0 => {
                    *focus = 0;
                    Out::Stay
                }
                1 if input.value == *device => Out::Install,
                2 => Out::Close,
                _ => Out::Stay,
            },
            Modal::Help => Out::Stay,
        }
    }

    pub fn captures_text(&self) -> bool {
        matches!(self, Modal::Erase { focus: 0, .. })
    }

    pub fn render(&mut self, c: &mut Canvas, screen: Rect) {
        let th = *c.th;
        let w = screen.width.saturating_sub(4).min(62);
        let inner_w = (w - 6) as usize;
        let bold = Style::new().fg(th.ink).add_modifier(Modifier::BOLD);
        let (title, body, extra): (String, Vec<String>, u16) = match self {
            Modal::Quit { .. } => {
                ("Quit the installer?".into(), wrap("Nothing has been written to the disk yet.", inner_w), 0)
            }
            Modal::Erase { device, model, beside: None, .. } => (
                format!("Erase {model}?"),
                wrap(&format!("Everything on {device} will be deleted. Type {device} to confirm."), inner_w),
                FIELD_ROWS + 1,
            ),
            Modal::Erase { device, beside: Some(what), .. } => (
                "Install beside Windows?".into(),
                wrap(&format!("{what} Type {device} to confirm."), inner_w),
                FIELD_ROWS + 1,
            ),
            Modal::Help => ("Keys".into(), Vec::new(), HELP.len() as u16 + 1),
        };
        let h = 2 + 2 + body.len() as u16 + 1 + extra + if matches!(self, Modal::Help) { 0 } else { 2 } + 1;
        let x = (screen.width.saturating_sub(w)) / 2;
        let y = (screen.height.saturating_sub(h)) / 2;
        let a = rect_in(screen, x, y, w, h);
        c.clear(a, Style::new().bg(th.surface).fg(th.ink));
        let b = fg(th.line).bg(th.surface);
        c.text(a, 0, 0, "╭", b);
        c.text(a, w - 1, 0, "╮", b);
        c.text(a, 0, h - 1, "╰", b);
        c.text(a, w - 1, h - 1, "╯", b);
        for i in 1..w - 1 {
            c.text(a, i, 0, "─", b);
            c.text(a, i, h - 1, "─", b);
        }
        for j in 1..h - 1 {
            c.text(a, 0, j, "│", b);
            c.text(a, w - 1, j, "│", b);
        }
        c.hit(a, W_MODAL, 99);
        c.text(a, 3, 2, &title, bold);
        let mut row = 4;
        for l in &body {
            c.text(a, 3, row, l, fg(th.muted));
            row += 1;
        }
        row += 1;
        let inner = rect_in(a, 3, 0, w - 6, h);
        match self {
            Modal::Quit { focus } => {
                let r = Rect { y: a.y + row, height: 1, ..inner };
                let bw = button(c, r, 0, "Quit", Kind::Secondary, *focus == 0, Hit { w: W_MODAL, i: 0 });
                button(c, r, bw + 2, "Keep installing", Kind::Primary, *focus == 1, Hit { w: W_MODAL, i: 1 });
            }
            Modal::Erase { input, focus, device, beside, .. } => {
                let ok = input.value == *device;
                let action = if beside.is_some() { "Install" } else { "Erase and install" };
                draw_field(
                    c,
                    Rect { y: a.y + row, height: FIELD_ROWS, ..inner },
                    Field {
                        label: "Device",
                        input,
                        placeholder: device,
                        focused: *focus == 0,
                        error: None,
                        hint: None,
                        w: W_MODAL,
                        i: 0,
                    },
                );
                let r = Rect { y: a.y + row + FIELD_ROWS + 1, height: 1, ..inner };
                let bw = button(
                    c,
                    r,
                    0,
                    action,
                    if ok { Kind::Amber } else { Kind::Disabled },
                    *focus == 1,
                    Hit { w: W_MODAL, i: 1 },
                );
                button(c, r, bw + 2, "Cancel", Kind::Secondary, *focus == 2, Hit { w: W_MODAL, i: 2 });
            }
            Modal::Help => {
                for (i, (k, d)) in HELP.iter().enumerate() {
                    let r = Rect { y: a.y + row - 1 + i as u16, height: 1, ..inner };
                    c.text(r, 0, 0, k, fg(th.ink));
                    c.text(r, 18, 0, d, fg(th.muted));
                }
            }
        }
    }
}

fn next(focus: usize, ok: bool, d: isize) -> usize {
    let stops: &[usize] = if ok { &[0, 1, 2] } else { &[0, 2] };
    let p = stops.iter().position(|&s| s == focus).unwrap_or(0) as isize;
    stops[(p + d).rem_euclid(stops.len() as isize) as usize]
}
