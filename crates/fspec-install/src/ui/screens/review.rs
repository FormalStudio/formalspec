use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use super::modal::Modal;
use super::{Flow, Hints, Shared, Step, hints};
use crate::model::Distro;
use crate::ui::theme::fg;
use crate::ui::widgets::controls::{Kind, button, toggle};
use crate::ui::widgets::{Canvas, Hit, rect_in, wrap};

const W_GROUP: u8 = 0;
const W_TOGGLE: u8 = 1;
const W_BUTTON: u8 = 2;

#[derive(Default, Clone)]
pub struct Summary {
    /// `(label, value)` for Language, Disk, System and Account, in that order.
    pub groups: Vec<(&'static str, String)>,
    pub device: String,
    pub model: String,
    pub distro: Option<&'static Distro>,
    /// What happens to Windows when installing beside it, and the confirm modal's sentence.
    pub windows: Option<Windows>,
}

#[derive(Default, Clone)]
pub struct Windows {
    pub review: String,
    pub confirm: String,
    pub bitlocker: bool,
}

#[derive(Default)]
pub struct Review {
    pub secure_boot: bool,
    pub summary: Summary,
    /// 0 is the Secure Boot toggle, 1 the install button.
    focus: usize,
}

impl Review {
    pub fn enter(&mut self, summary: Summary) {
        self.summary = summary;
        self.focus = 1;
    }

    pub fn captures_text(&self) -> bool {
        false
    }

    /// AerynOS has no sbctl hooks, so its first kernel update would boot unsigned.
    fn is_aerynos(&self) -> bool {
        self.summary.distro.is_some_and(|d| d.id == "aerynos")
    }

    fn can_toggle(&self, sh: &Shared) -> bool {
        sh.firmware.efi && sh.firmware.setup_mode && !self.is_aerynos()
    }

    fn target(&self, n: usize, sh: &Shared) -> Step {
        match n {
            0 => Step::Language,
            1 => Step::Disk,
            2 if sh.restore => Step::Host,
            2 => Step::System,
            _ => Step::Account,
        }
    }

    /// AerynOS's blsforme puts its kernels on the ESP, which beside Windows is Windows' own.
    fn blocked(&self) -> bool {
        self.summary.windows.is_some() && self.summary.distro.is_some_and(|d| d.id == "aerynos")
    }

    fn erase(&self) -> Flow {
        if self.blocked() {
            return Flow::Stay;
        }
        let beside = self.summary.windows.as_ref().map(|w| w.confirm.clone());
        Flow::Modal(Modal::erase(&self.summary.device, &self.summary.model, beside))
    }

    pub fn key(&mut self, k: &KeyEvent, sh: &mut Shared) -> Flow {
        match k.code {
            KeyCode::Char(c @ '1'..='4') => return Flow::Jump(self.target(c as usize - '1' as usize, sh)),
            KeyCode::Char(' ') if self.can_toggle(sh) => self.secure_boot = !self.secure_boot,
            KeyCode::Tab | KeyCode::Down | KeyCode::Up | KeyCode::BackTab => self.focus = 1 - self.focus,
            KeyCode::Enter => return self.erase(),
            KeyCode::Esc => return Flow::Back,
            _ => {}
        }
        Flow::Stay
    }

    pub fn click(&mut self, h: Hit, sh: &Shared) -> Flow {
        match h.w {
            W_GROUP => return Flow::Jump(self.target(h.i, sh)),
            W_TOGGLE => {
                self.focus = 0;
                if self.can_toggle(sh) {
                    self.secure_boot = !self.secure_boot;
                }
            }
            W_BUTTON => return self.erase(),
            _ => {}
        }
        Flow::Stay
    }

    pub fn hints(&self) -> Hints {
        hints(&[("1-4", "Change"), ("Space", "Toggle"), ("Tab", "Next"), ("Enter", "Install"), ("Esc", "Back")])
    }

    pub fn render(&mut self, c: &mut Canvas, a: Rect, sh: &Shared) {
        let th = *c.th;
        let bold = Style::new().fg(th.ink).add_modifier(Modifier::BOLD);
        let w = a.width.min(84);
        c.text(a, 0, 0, "Review", bold);
        let label_w = 11;
        for (i, (label, value)) in self.summary.groups.iter().enumerate() {
            let row = rect_in(a, 0, 2 + i as u16, w, 1);
            c.text(row, 0, 0, &format!("{}", i + 1), fg(th.faint));
            c.text(row, 3, 0, label, fg(th.muted));
            let room = Rect { width: row.width.saturating_sub(8), ..row };
            c.text_ell(room, 3 + label_w, 0, value, fg(th.ink));
            c.text_right(row, 0, "Change", fg(th.muted));
            c.hit(row, W_GROUP, i);
        }
        let mut y = 2 + self.summary.groups.len() as u16 + 1;
        let note_w = (w as usize).saturating_sub(3);
        if let Some(win) = &self.summary.windows {
            let mut lines = vec![(win.review.clone(), th.ink)];
            if win.bitlocker {
                lines.push(("Have your BitLocker recovery key ready; Windows may ask for it once.".into(), th.muted));
            }
            if self.blocked() {
                lines.push(("AerynOS cannot install beside Windows yet.".into(), th.amber));
            }
            for (text, color) in lines {
                for l in wrap(&text, note_w) {
                    c.text(a, 3, y, &l, fg(color));
                    y += 1;
                }
            }
            y += 1;
        }
        let enabled = self.can_toggle(sh);
        let on = self.secure_boot && enabled;
        toggle(
            c,
            rect_in(a, 0, y, w, 1),
            on,
            "Use Secure Boot with your own keys",
            self.focus == 0,
            enabled,
            Hit { w: W_TOGGLE, i: 0 },
        );
        y += 1;
        let note_w = (w as usize).saturating_sub(7);
        let name = self.summary.distro.map_or("This system", |d| d.name);
        let mut notes: Vec<(String, bool)> = Vec::new();
        if !sh.firmware.efi {
            notes.push(("This machine booted without UEFI, so Secure Boot is unavailable.".into(), false));
        } else if !sh.firmware.setup_mode {
            notes.push((
                "To turn this on, open your firmware settings, clear the Secure Boot keys to enter setup mode, then start the installer again.".into(),
                false,
            ));
        } else if self.is_aerynos() {
            notes.push((
                "AerynOS cannot use Secure Boot with your own keys: its kernel updates would not be signed.".into(),
                false,
            ));
        } else if on {
            notes.push((format!("{name}'s boot manager and kernels are signed with your own key."), false));
            notes.push(("Measured boot and anti-cheat on a self-signed chain are not verified yet.".into(), true));
        }
        for (text, faint) in notes {
            for l in wrap(&text, note_w) {
                c.text(a, 7, y, &l, if faint { fg(th.faint) } else { fg(th.muted) });
                y += 1;
            }
        }
        y += 1;
        let row = rect_in(a, 0, y, a.width, 1);
        let (label, kind) = match &self.summary.windows {
            _ if self.blocked() => ("Install beside Windows".to_string(), Kind::Disabled),
            Some(_) => ("Install beside Windows".to_string(), Kind::Primary),
            None => (format!("Erase {} and install", self.summary.device), Kind::Amber),
        };
        button(c, row, 0, &label, kind, self.focus == 1, Hit { w: W_BUTTON, i: 0 });
    }
}
