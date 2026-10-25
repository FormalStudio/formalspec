use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use super::{Flow, Hints, Shared, hints};
use crate::catalog::DISTROS;
use crate::model::{Delivery, Distro, Release};
use crate::ui::theme::fg;
use crate::ui::widgets::controls::chip;
use crate::ui::widgets::list::{FilterList, row_chrome};
use crate::ui::widgets::{Canvas, Hit};

const W_LIST: u8 = 0;

/// The two chips every row shows: release model, then how the system is delivered.
fn tags(d: &Distro) -> [&'static str; 2] {
    [
        match d.release {
            Release::Rolling => "rolling",
            Release::Fixed => "fixed",
        },
        match d.delivery {
            Delivery::Image => "image",
            Delivery::Atomic => "atomic",
            Delivery::Classic => "classic",
        },
    ]
}

/// Room for the longest name and its prerelease chip, so the tag chips line up in columns.
fn name_col() -> u16 {
    let chip = |d: &Distro| d.prerelease.map_or(0, |p| if p == "release candidate" { 5 } else { p.len() + 3 });
    DISTROS.iter().map(|d| d.name.len() + chip(d)).max().unwrap_or(0) as u16 + 2
}

pub struct System {
    list: FilterList,
    height: usize,
    /// The machine booted with UEFI, so distros that need it can be picked.
    efi: bool,
}

impl Default for System {
    fn default() -> Self {
        Self::new()
    }
}

impl System {
    pub fn new() -> Self {
        System { list: FilterList::new(DISTROS.len()), height: 6, efi: true }
    }

    pub fn current(&self) -> Option<&'static Distro> {
        self.list.selected().map(|i| &DISTROS[i])
    }

    pub fn captures_text(&self) -> bool {
        false
    }

    fn usable(&self, i: usize) -> bool {
        self.efi || !DISTROS[i].needs_efi
    }

    pub fn key(&mut self, k: &KeyEvent, sh: &mut Shared) -> Flow {
        self.efi = sh.firmware.efi;
        match k.code {
            KeyCode::Enter if self.list.selected().is_some_and(|i| self.usable(i)) => Flow::Next,
            KeyCode::Enter => Flow::Stay,
            KeyCode::Esc => Flow::Back,
            _ => {
                let efi = self.efi;
                self.list.nav(k, self.height, true, &|i| efi || !DISTROS[i].needs_efi);
                Flow::Stay
            }
        }
    }

    pub fn click(&mut self, h: Hit) -> Flow {
        if h.w != W_LIST || !self.usable(h.i) {
            return Flow::Stay;
        }
        if self.list.selected() == Some(h.i) {
            return Flow::Next;
        }
        self.list.select_item(h.i);
        Flow::Stay
    }

    pub fn wheel(&mut self, _w: u8, d: isize) {
        self.list.wheel(d, self.height);
    }

    pub fn hints(&self) -> Hints {
        hints(&[("↑↓", "Move"), ("Enter", "Continue"), ("Esc", "Back"), ("?", "Keys")])
    }

    pub fn render(&mut self, c: &mut Canvas, a: Rect, sh: &Shared) {
        self.efi = sh.firmware.efi;
        let th = *c.th;
        let Some(d) = self.current() else { return };
        c.text(a, 0, 0, "Choose a system", Style::new().fg(th.ink).add_modifier(Modifier::BOLD));
        let needs_uefi = d.needs_efi && !self.efi;
        let detail_h = 2 + usize::from(needs_uefi);
        let list_h = (a.height as usize).saturating_sub(2 + 1 + detail_h).max(3).min(DISTROS.len());
        self.height = list_h;
        let top = self.list.window(list_h);
        for (n, idx) in (top..DISTROS.len()).take(list_h).enumerate() {
            let dd = &DISTROS[idx];
            let row = Rect { y: a.y + 2 + n as u16, height: 1, width: a.width, ..a };
            let sel = self.list.selected() == Some(idx);
            row_chrome(c, row, sel, true);
            let st = match (sel, self.usable(idx)) {
                (_, false) => fg(th.faint),
                (true, true) => Style::new().fg(th.ink).add_modifier(Modifier::BOLD),
                (false, true) => fg(th.ink),
            };
            let used = c.text(row, 2, 0, dd.name, st);
            if let Some(stage) = dd.prerelease {
                let short = if stage == "release candidate" { "RC" } else { stage };
                c.text(row, 2 + used + 1, 0, &format!(" {short} "), fg(th.amber).bg(th.surface));
            }
            let mut x = 2 + name_col();
            for t in tags(dd) {
                chip(c, row, x, t);
                x += 9;
            }
            c.hit(row, W_LIST, idx);
        }
        let y = 2 + list_h as u16 + 1;
        c.text(a, 0, y, d.name, Style::new().fg(th.ink).add_modifier(Modifier::BOLD));
        c.text(a, 0, y + 1, d.reason, fg(th.muted));
        if needs_uefi {
            c.text(a, 0, y + 2, "Needs UEFI; this machine booted with BIOS.", fg(th.amber));
        }
    }
}
