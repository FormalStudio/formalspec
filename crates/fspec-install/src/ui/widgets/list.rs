use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::Style;

use super::Canvas;
use crate::ui::theme::fg;

/// Selection, scroll and type-to-filter state over a list the screen owns.
#[derive(Clone, Debug, Default)]
pub struct FilterList {
    pub filter: String,
    /// Position in `vis`.
    pub sel: usize,
    pub top: usize,
    /// Indexes into the screen's items that match the filter.
    pub vis: Vec<usize>,
    follow: bool,
}

impl FilterList {
    pub fn new(len: usize) -> Self {
        FilterList { vis: (0..len).collect(), follow: true, ..Default::default() }
    }

    pub fn selected(&self) -> Option<usize> {
        self.vis.get(self.sel).copied()
    }

    pub fn select_item(&mut self, item: usize) {
        if let Some(p) = self.vis.iter().position(|&i| i == item) {
            self.sel = p;
            self.follow = true;
        }
    }

    pub fn refilter<'a>(&mut self, labels: impl Iterator<Item = &'a str>) {
        let keep = self.selected();
        let f = self.filter.to_lowercase();
        let mut hits: Vec<(u8, usize)> = labels
            .enumerate()
            .filter_map(|(i, l)| {
                let l = l.to_lowercase();
                let rank = if f.is_empty() || l.starts_with(&f) {
                    0
                } else if l.split(|c: char| !c.is_alphanumeric()).any(|w| w.starts_with(&f)) {
                    1
                } else if l.contains(&f) {
                    2
                } else {
                    return None;
                };
                Some((rank, i))
            })
            .collect();
        hits.sort();
        self.vis = hits.into_iter().map(|(_, i)| i).collect();
        // Typing jumps to the best match; clearing the filter keeps what was picked.
        self.sel = if f.is_empty() { keep.and_then(|k| self.vis.iter().position(|&i| i == k)).unwrap_or(0) } else { 0 };
        self.top = 0;
        self.follow = true;
    }

    /// Moves by `d` rows to the nearest row `ok` accepts; stays put when there is none.
    pub fn step(&mut self, d: isize, ok: &dyn Fn(usize) -> bool) {
        if self.vis.is_empty() {
            return;
        }
        let mut p = self.sel as isize;
        let last = self.vis.len() as isize - 1;
        let dir = d.signum();
        let mut left = d.abs();
        while left > 0 {
            let mut n = p + dir;
            while (0..=last).contains(&n) && !ok(self.vis[n as usize]) {
                n += dir;
            }
            if !(0..=last).contains(&n) {
                break;
            }
            p = n;
            left -= 1;
        }
        self.sel = p as usize;
        self.follow = true;
    }

    pub fn home(&mut self, ok: &dyn Fn(usize) -> bool) {
        self.sel = 0;
        self.step(0, ok);
        if !self.vis.first().is_some_and(|&i| ok(i)) {
            self.step(1, ok);
        }
    }

    pub fn end(&mut self, ok: &dyn Fn(usize) -> bool) {
        self.sel = self.vis.len().saturating_sub(1);
        if !self.vis.get(self.sel).is_some_and(|&i| ok(i)) {
            self.step(-1, ok);
        }
        self.follow = true;
    }

    pub fn wheel(&mut self, d: isize, height: usize) {
        let max = self.vis.len().saturating_sub(height);
        self.top = (self.top as isize + d).clamp(0, max as isize) as usize;
        self.follow = false;
    }

    /// Navigation keys. Returns whether the key moved the selection.
    pub fn nav(&mut self, k: &KeyEvent, height: usize, vim: bool, ok: &dyn Fn(usize) -> bool) -> bool {
        let page = height.max(2) as isize - 1;
        match k.code {
            KeyCode::Up => self.step(-1, ok),
            KeyCode::Down => self.step(1, ok),
            KeyCode::Char('k') if vim => self.step(-1, ok),
            KeyCode::Char('j') if vim => self.step(1, ok),
            KeyCode::PageUp => self.step(-page, ok),
            KeyCode::PageDown => self.step(page, ok),
            KeyCode::Home => self.home(ok),
            KeyCode::End => self.end(ok),
            _ => return false,
        }
        true
    }

    /// Type-to-filter editing. Returns whether the filter changed.
    pub fn edit_filter(&mut self, k: &KeyEvent) -> bool {
        match k.code {
            KeyCode::Char(c) if !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                self.filter.push(c)
            }
            KeyCode::Backspace if !self.filter.is_empty() => {
                self.filter.pop();
            }
            _ => return false,
        }
        true
    }

    /// Keeps the selection in the window of `height` rows and returns the first visible position.
    pub fn window(&mut self, height: usize) -> usize {
        let max = self.vis.len().saturating_sub(height);
        if self.follow {
            if self.sel < self.top {
                self.top = self.sel;
            } else if height > 0 && self.sel >= self.top + height {
                self.top = self.sel + 1 - height;
            }
        }
        self.top = self.top.min(max);
        self.top
    }
}

/// Selected-row chrome: surface fill and a 1-col accent bar.
pub fn row_chrome(c: &mut Canvas, row: Rect, selected: bool, focused: bool) {
    if !selected {
        return;
    }
    let th = *c.th;
    c.fill(row, Style::new().bg(th.sel));
    c.text(row, 0, 0, "▌", fg(if focused { th.accent } else { th.faint }));
}
