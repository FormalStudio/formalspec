use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use super::{Flow, Hints, Shared, hints, tab_focus};
use crate::model::Locale;
use crate::ui::theme::fg;
use crate::ui::widgets::input::{FIELD_ROWS, Field, TextInput, draw_field, draw_filter};
use crate::ui::widgets::list::{FilterList, row_chrome};
use crate::ui::widgets::{Canvas, Hit, rect_in};

const TEST: usize = 3;

pub struct Language {
    locales: Vec<(String, String)>,
    labels: Vec<String>,
    keymaps: Vec<(String, String)>,
    keymap_labels: Vec<String>,
    zones: Vec<String>,
    lists: [FilterList; 3],
    focus: usize,
    test: TextInput,
    height: usize,
}

impl Language {
    pub fn new(sh: &Shared) -> Self {
        let locales = sh.be.locales();
        let keymaps = sh.be.keymaps();
        let zones = sh.be.timezones();
        let labels = locales.iter().map(|(l, n)| format!("{n} {l}")).collect();
        let keymap_labels = keymaps.iter().map(|(k, d)| format!("{k} {d}")).collect();
        let mut lists = [FilterList::new(locales.len()), FilterList::new(keymaps.len()), FilterList::new(zones.len())];
        if let Some(i) = locales.iter().position(|(l, _)| l == "en_US.UTF-8") {
            lists[0].select_item(i);
        }
        if let Some(i) = keymaps.iter().position(|(k, _)| k == "us") {
            lists[1].select_item(i);
        }
        if let Some(i) = zones.iter().position(|z| z == "UTC") {
            lists[2].select_item(i);
        }
        Language {
            locales,
            labels,
            keymaps,
            keymap_labels,
            zones,
            lists,
            focus: 0,
            test: TextInput::default(),
            height: 10,
        }
    }

    pub fn value(&self) -> Locale {
        let pick = |l: &FilterList, n: usize| l.selected().filter(|&i| i < n);
        Locale {
            locale: pick(&self.lists[0], self.locales.len())
                .map_or_else(|| "en_US.UTF-8".into(), |i| self.locales[i].0.clone()),
            keymap: pick(&self.lists[1], self.keymaps.len()).map_or_else(|| "us".into(), |i| self.keymaps[i].0.clone()),
            timezone: pick(&self.lists[2], self.zones.len()).map_or_else(|| "UTC".into(), |i| self.zones[i].clone()),
        }
    }

    pub fn describe(&self) -> String {
        let v = self.value();
        let name = self.locales.iter().find(|(l, _)| *l == v.locale).map_or(v.locale.as_str(), |(_, n)| n.as_str());
        format!("{name} · {} · {}", v.keymap, v.timezone)
    }

    pub fn apply_keymap(&self, sh: &Shared) {
        let _ = sh.be.set_keymap(&self.value().keymap);
    }

    pub fn captures_text(&self) -> bool {
        true
    }

    fn refilter(&mut self, col: usize) {
        match col {
            0 => self.lists[0].refilter(self.labels.iter().map(String::as_str)),
            1 => self.lists[1].refilter(self.keymap_labels.iter().map(String::as_str)),
            _ => self.lists[2].refilter(self.zones.iter().map(String::as_str)),
        }
    }

    /// The layout loads when focus leaves the keyboard column or the step, never while typing in
    /// its filter: loading it mid-filter would remap the very keys being typed.
    pub fn key(&mut self, k: &KeyEvent, sh: &mut Shared) -> Flow {
        let was = self.focus;
        let flow = self.key_inner(k);
        if was == 1 && (self.focus != 1 || matches!(flow, Flow::Next)) {
            self.apply_keymap(sh);
        }
        flow
    }

    fn key_inner(&mut self, k: &KeyEvent) -> Flow {
        if let Some(f) = tab_focus(k, self.focus, 4) {
            self.focus = f;
            return Flow::Stay;
        }
        match k.code {
            KeyCode::Enter => return Flow::Next,
            KeyCode::Esc => {
                if self.focus < TEST && !self.lists[self.focus].filter.is_empty() {
                    self.lists[self.focus].filter.clear();
                    self.refilter(self.focus);
                    return Flow::Stay;
                }
                return Flow::Back;
            }
            _ => {}
        }
        if self.focus == TEST {
            if matches!(k.code, KeyCode::Up) {
                self.focus = 2;
            } else {
                self.test.key(k);
            }
            return Flow::Stay;
        }
        match k.code {
            KeyCode::Left if self.focus > 0 => self.focus -= 1,
            KeyCode::Right if self.focus < 2 => self.focus += 1,
            KeyCode::Down if self.lists[self.focus].selected() == self.lists[self.focus].vis.last().copied() => {
                self.focus = TEST
            }
            _ => {
                let col = self.focus;
                if !self.lists[col].nav(k, self.height, false, &|_| true) && self.lists[col].edit_filter(k) {
                    self.refilter(col);
                }
            }
        }
        Flow::Stay
    }

    pub fn click(&mut self, h: Hit, sh: &Shared) -> Flow {
        if self.focus == 1 && h.w != 1 {
            self.apply_keymap(sh);
        }
        if h.w as usize == TEST {
            self.focus = TEST;
            return Flow::Stay;
        }
        let col = h.w as usize;
        if col > 2 {
            return Flow::Stay;
        }
        self.focus = col;
        if self.lists[col].selected() == Some(h.i) {
            return Flow::Next;
        }
        self.lists[col].select_item(h.i);
        if col == 1 {
            self.apply_keymap(sh);
        }
        Flow::Stay
    }

    pub fn wheel(&mut self, w: u8, d: isize) {
        if let Some(l) = self.lists.get_mut(w as usize) {
            l.wheel(d, self.height);
        }
    }

    pub fn hints(&self) -> Hints {
        hints(&[("↑↓", "Move"), ("Tab", "Next column"), ("Enter", "Continue"), ("Esc", "Back"), ("?", "Keys")])
    }

    pub fn render(&mut self, c: &mut Canvas, a: Rect, _sh: &Shared) {
        let th = *c.th;
        let gap = 2;
        let usable = a.width.saturating_sub(gap * 2);
        let w0 = usable * 42 / 100;
        let w1 = usable * 24 / 100;
        let w2 = usable - w0 - w1;
        let xs = [0, w0 + gap, w0 + w1 + gap * 2];
        let ws = [w0, w1, w2];
        let titles = ["Language", "Keyboard", "Time zone"];
        let list_top = 3;
        let list_h = a.height.saturating_sub(list_top + FIELD_ROWS + 1);
        self.height = list_h as usize;
        for col in 0..3 {
            let focused = self.focus == col;
            let ca = rect_in(a, xs[col], 0, ws[col], a.height);
            let head = if focused {
                Style::new().fg(th.accent).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(th.muted).add_modifier(Modifier::BOLD)
            };
            c.text(ca, 0, 0, titles[col], head);
            draw_filter(c, Rect { y: ca.y + 1, height: 1, ..ca }, &self.lists[col].filter, focused);
            let top = self.lists[col].window(list_h as usize);
            let items: Vec<usize> = self.lists[col].vis.iter().copied().skip(top).take(list_h as usize).collect();
            for (n, item) in items.iter().enumerate() {
                let row = Rect { y: ca.y + list_top + n as u16, height: 1, ..ca };
                let sel = self.lists[col].selected() == Some(*item);
                row_chrome(c, row, sel, focused);
                let label = match col {
                    0 => self.locales[*item].1.as_str(),
                    1 => self.keymaps[*item].1.as_str(),
                    _ => self.zones[*item].as_str(),
                };
                let st = if sel { Style::new().fg(th.ink).add_modifier(Modifier::BOLD) } else { fg(th.ink) };
                c.text_ell(row, 2, 0, label, st);
                c.hit(row, col as u8, *item);
            }
            if self.lists[col].vis.is_empty() {
                c.text(ca, 2, list_top, "No matches", fg(th.faint));
            }
        }
        let fa = rect_in(a, 0, a.height.saturating_sub(FIELD_ROWS), a.width.min(48), FIELD_ROWS);
        draw_field(
            c,
            fa,
            Field {
                label: "Test your keyboard",
                input: &self.test,
                placeholder: "Type here",
                focused: self.focus == TEST,
                error: None,
                hint: None,
                w: TEST as u8,
                i: 0,
            },
        );
    }
}
