use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use super::{Flow, Hints, Shared, hints, plain};
use crate::model::{DiskChoice, DiskInfo, Secret};
use crate::ui::theme::fg;
use crate::ui::widgets::controls::{chip, segmented, stepper, toggle};
use crate::ui::widgets::input::{FIELD_ROWS, Field, TextInput, draw_field};
use crate::ui::widgets::list::{FilterList, row_chrome};
use crate::ui::widgets::{Canvas, Hit, fmt_size, rect_in, wrap};
use unicode_width::UnicodeWidthStr;

const W_LIST: u8 = 0;
const W_TOGGLE: u8 = 1;
const W_PASS: u8 = 2;
const W_CONFIRM: u8 = 3;
const W_SWAP: u8 = 4;
const W_MODE: u8 = 5;
const W_SIZE: u8 = 6;
const EFI_BYTES: u64 = 2_000_000_000;
const MAX_SWAP: u64 = 64;
const GB: u64 = fspec_switch::GB;
/// The size picker moves in these steps.
const SIZE_STEP: u64 = 8;
/// What the picker starts at when Windows can give that much.
const DEFAULT_LINUX: u64 = 128;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Focus {
    List,
    Mode,
    Size,
    Toggle,
    Pass,
    Confirm,
    Swap,
}

/// Where Linux goes on a disk holding Windows.
enum Room {
    /// Free space already there, in bytes.
    Free(u64),
    /// Shrunk off Windows; the most it can give, in GB.
    Shrink(u64),
    /// Neither, and why.
    None(String),
}

fn room(w: &fspec_switch::WindowsDisk) -> Room {
    if w.free() >= fspec_switch::MIN_LINUX {
        return Room::Free(w.free());
    }
    match w.max_linux() {
        Ok(most) => Room::Shrink(most / GB),
        Err(why) => Room::None(why),
    }
}

pub struct DiskScreen {
    disks: Vec<DiskInfo>,
    list: FilterList,
    encrypt: bool,
    pass: TextInput,
    confirm: TextInput,
    swap: u64,
    /// Install beside Windows rather than erase the disk, when it holds Windows.
    beside: bool,
    /// What Linux takes from Windows, in GB.
    linux_gb: u64,
    /// The disk `beside` and `linux_gb` were set for.
    for_disk: Option<String>,
    focus: Focus,
    attempted: bool,
    height: usize,
}

impl Default for DiskScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl DiskScreen {
    pub fn new() -> Self {
        DiskScreen {
            disks: Vec::new(),
            list: FilterList::new(0),
            encrypt: false,
            pass: TextInput::new(true),
            confirm: TextInput::new(true),
            swap: 4,
            beside: true,
            linux_gb: DEFAULT_LINUX,
            for_disk: None,
            focus: Focus::List,
            attempted: false,
            height: 4,
        }
    }

    pub fn reload(&mut self, sh: &Shared) {
        let keep = self.selected().map(|d| d.path.clone());
        self.disks = sh.be.disks().unwrap_or_default();
        self.list = FilterList::new(self.disks.len());
        let ok = |i: usize| !self.disks[i].live_medium;
        match keep.and_then(|p| self.disks.iter().position(|d| d.path == p)) {
            Some(i) => self.list.select_item(i),
            None => self.list.home(&ok),
        }
        self.follow();
    }

    /// A newly picked disk starts beside Windows, with the default size.
    fn follow(&mut self) {
        let path = self.selected().map(|d| d.path.clone());
        if path == self.for_disk {
            return;
        }
        self.for_disk = path;
        self.beside = true;
        if let Some(Room::Shrink(most)) = self.windows().map(room) {
            self.linux_gb = DEFAULT_LINUX.clamp(fspec_switch::MIN_LINUX / GB, most);
        }
    }

    fn windows(&self) -> Option<&fspec_switch::WindowsDisk> {
        self.selected().and_then(|d| d.windows.as_ref())
    }

    /// The Windows disk when installing beside it.
    fn beside_windows(&self) -> Option<&fspec_switch::WindowsDisk> {
        self.windows().filter(|_| self.beside)
    }

    fn resize(&mut self, d: i64) {
        if let Some(Room::Shrink(most)) = self.windows().map(room) {
            let min = fspec_switch::MIN_LINUX / GB;
            self.linux_gb = (self.linux_gb as i64 + d * SIZE_STEP as i64).clamp(min as i64, most as i64) as u64;
        }
    }

    /// Where Linux goes beside Windows, or why it cannot.
    fn space(&self) -> Option<Result<fspec_switch::Space, String>> {
        let w = self.beside_windows()?;
        Some(match room(w) {
            Room::Free(_) => fspec_switch::space(w, None).map_err(|e| e.to_string()),
            Room::Shrink(_) => fspec_switch::space(w, Some(self.linux_gb * GB)).map_err(|e| e.to_string()),
            Room::None(why) => Err(why),
        })
    }

    pub fn selected(&self) -> Option<&DiskInfo> {
        self.list.selected().map(|i| &self.disks[i]).filter(|d| !d.live_medium)
    }

    pub fn captures_text(&self) -> bool {
        matches!(self.focus, Focus::Pass | Focus::Confirm)
    }

    fn pass_error(&self) -> Option<&'static str> {
        let show = self.attempted || (!self.pass.is_empty() && self.focus != Focus::Pass);
        (self.encrypt && show && self.pass.len() < 8).then_some("Use at least 8 characters.")
    }

    fn confirm_error(&self) -> Option<&'static str> {
        let show = self.attempted || (!self.confirm.is_empty() && self.focus != Focus::Confirm);
        (self.encrypt && show && self.confirm.value != self.pass.value).then_some("The passphrases do not match.")
    }

    fn valid(&self) -> bool {
        self.selected().is_some()
            && !matches!(self.space(), Some(Err(_)))
            && (!self.encrypt || (self.pass.len() >= 8 && self.pass.value == self.confirm.value))
    }

    pub fn value(&self) -> Option<DiskChoice> {
        if !self.valid() {
            return None;
        }
        Some(DiskChoice {
            device: self.selected()?.path.clone(),
            encrypt: self.encrypt.then(|| Secret(self.pass.value.clone())),
            swap_gib: self.swap,
            windows: match (self.beside_windows(), self.space()) {
                (Some(w), Some(Ok(space))) => Some(fspec_switch::BesideWindows { disk: w.clone(), space }),
                _ => None,
            },
        })
    }

    fn stops(&self) -> Vec<Focus> {
        let mut s = vec![Focus::List];
        if let Some(w) = self.windows() {
            s.push(Focus::Mode);
            if self.beside && matches!(room(w), Room::Shrink(_)) {
                s.push(Focus::Size);
            }
        }
        s.push(Focus::Toggle);
        if self.encrypt {
            s.extend([Focus::Pass, Focus::Confirm]);
        }
        s.push(Focus::Swap);
        s
    }

    fn shift(&mut self, d: isize) {
        let s = self.stops();
        let p = s.iter().position(|f| *f == self.focus).unwrap_or(0) as isize;
        self.focus = s[(p + d).rem_euclid(s.len() as isize) as usize];
    }

    fn set_encrypt(&mut self, on: bool) {
        self.encrypt = on;
        if !on && matches!(self.focus, Focus::Pass | Focus::Confirm) {
            self.focus = Focus::Toggle;
        }
    }

    fn proceed(&mut self) -> Flow {
        if self.valid() {
            return Flow::Next;
        }
        self.attempted = true;
        if self.selected().is_none() {
            self.focus = Focus::List;
        } else if matches!(self.space(), Some(Err(_))) {
            self.focus = Focus::Mode;
        } else if self.pass_error().is_some() {
            self.focus = Focus::Pass;
        } else {
            self.focus = Focus::Confirm;
        }
        Flow::Stay
    }

    pub fn key(&mut self, k: &KeyEvent, sh: &mut Shared) -> Flow {
        match k.code {
            KeyCode::Tab => {
                self.shift(1);
                return Flow::Stay;
            }
            KeyCode::BackTab => {
                self.shift(-1);
                return Flow::Stay;
            }
            KeyCode::Enter => return self.proceed(),
            KeyCode::Esc => return Flow::Back,
            _ => {}
        }
        let flow = self.key_in(k, sh);
        self.follow();
        flow
    }

    fn key_in(&mut self, k: &KeyEvent, sh: &mut Shared) -> Flow {
        match self.focus {
            Focus::List => {
                let disks = &self.disks;
                let ok = |i: usize| !disks[i].live_medium;
                if k.code == KeyCode::Down && self.list.sel + 1 >= self.list.vis.len() {
                    self.shift(1);
                } else if !self.list.nav(k, self.height, true, &ok) {
                    match k.code {
                        KeyCode::Char('r') if plain(k) => self.reload(sh),
                        KeyCode::Char(' ') => self.set_encrypt(!self.encrypt),
                        _ => {}
                    }
                }
            }
            Focus::Mode => match k.code {
                KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right => self.beside = !self.beside,
                KeyCode::Up => self.shift(-1),
                KeyCode::Down => self.shift(1),
                _ => {}
            },
            Focus::Size => match k.code {
                KeyCode::Left | KeyCode::Char('h') => self.resize(-1),
                KeyCode::Right | KeyCode::Char('l') => self.resize(1),
                KeyCode::Up => self.shift(-1),
                KeyCode::Down => self.shift(1),
                _ => {}
            },
            Focus::Toggle => match k.code {
                KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right => self.set_encrypt(!self.encrypt),
                KeyCode::Up => self.shift(-1),
                KeyCode::Down => self.shift(1),
                _ => {}
            },
            Focus::Pass | Focus::Confirm => match k.code {
                KeyCode::Up => self.shift(-1),
                KeyCode::Down => self.shift(1),
                _ => {
                    let input = if self.focus == Focus::Pass { &mut self.pass } else { &mut self.confirm };
                    input.key(k);
                }
            },
            Focus::Swap => match k.code {
                KeyCode::Left | KeyCode::Char('h') => self.swap = self.swap.saturating_sub(1),
                KeyCode::Right | KeyCode::Char('l') => self.swap = (self.swap + 1).min(MAX_SWAP),
                KeyCode::Up => self.shift(-1),
                _ => {}
            },
        }
        Flow::Stay
    }

    pub fn click(&mut self, h: Hit) -> Flow {
        match h.w {
            W_LIST => {
                if self.disks.get(h.i).is_none_or(|d| d.live_medium) {
                    return Flow::Stay;
                }
                self.focus = Focus::List;
                if self.list.selected() == Some(h.i) {
                    return self.proceed();
                }
                self.list.select_item(h.i);
                self.follow();
            }
            W_MODE => {
                self.focus = Focus::Mode;
                self.beside = h.i == 0;
            }
            W_SIZE => {
                self.focus = Focus::Size;
                self.resize(if h.i == 0 { -1 } else { 1 });
            }
            W_TOGGLE => {
                self.focus = Focus::Toggle;
                self.set_encrypt(!self.encrypt);
            }
            W_PASS => self.focus = Focus::Pass,
            W_CONFIRM => self.focus = Focus::Confirm,
            W_SWAP => {
                self.focus = Focus::Swap;
                self.swap = if h.i == 0 { self.swap.saturating_sub(1) } else { (self.swap + 1).min(MAX_SWAP) };
            }
            _ => {}
        }
        Flow::Stay
    }

    pub fn wheel(&mut self, w: u8, d: isize) {
        if w == W_LIST {
            self.list.wheel(d, self.height);
        }
    }

    pub fn hints(&self) -> Hints {
        match self.focus {
            Focus::List => hints(&[
                ("↑↓", "Move"),
                ("Space", "Encrypt"),
                ("r", "Rescan"),
                ("Enter", "Continue"),
                ("Esc", "Back"),
                ("?", "Keys"),
            ]),
            Focus::Swap | Focus::Size => {
                hints(&[("←→", "Change size"), ("Tab", "Next"), ("Enter", "Continue"), ("Esc", "Back")])
            }
            Focus::Mode => hints(&[("←→", "Choose"), ("Tab", "Next"), ("Enter", "Continue"), ("Esc", "Back")]),
            Focus::Pass | Focus::Confirm => hints(&[("Tab", "Next"), ("Enter", "Continue"), ("Esc", "Back")]),
            _ => hints(&[("Tab", "Next"), ("Space", "Toggle"), ("Enter", "Continue"), ("Esc", "Back")]),
        }
    }

    pub fn render(&mut self, c: &mut Canvas, a: Rect, sh: &Shared) {
        let th = *c.th;
        let bold = Style::new().fg(th.ink).add_modifier(Modifier::BOLD);
        c.text(a, 0, 0, "Choose a disk", bold);
        if self.disks.is_empty() {
            c.text(a, 0, 2, "No disks found", bold);
            c.text(a, 0, 3, "Connect a disk of 16 GB or more, then press r to look again.", fg(th.muted));
            return;
        }
        let enc_rows = if self.encrypt { FIELD_ROWS + 1 } else { 0 };
        let win_rows = if self.windows().is_some() { 3 } else { 0 };
        let fixed = 2 + 1 + 2 + 1 + 1 + 1 + 1 + enc_rows + 1 + win_rows;
        let list_h = (a.height.saturating_sub(fixed) as usize).clamp(1, self.disks.len());
        self.height = list_h;
        let top = self.list.window(list_h);
        let size_w = 6;
        let chip_w = self.disks.iter().map(|d| d.transport.len() as u16).max().unwrap_or(4) + 2;
        let model_w = 20.min(a.width.saturating_sub(size_w + chip_w + 24));
        for (n, idx) in self.list.vis.clone().into_iter().skip(top).take(list_h).enumerate() {
            let d = &self.disks[idx];
            let row = Rect { y: a.y + 2 + n as u16, height: 1, ..a };
            let sel = self.list.selected() == Some(idx) && !d.live_medium;
            row_chrome(c, row, sel, self.focus == Focus::List);
            let name = if sel {
                Style::new().fg(th.ink).add_modifier(Modifier::BOLD)
            } else if d.live_medium {
                fg(th.faint)
            } else {
                fg(th.ink)
            };
            c.text_ell(row, 2, 0, &d.model, name);
            let size = fmt_size(d.bytes);
            let sx = 2 + model_w + 1 + size_w.saturating_sub(size.len() as u16);
            c.text(row, sx, 0, &size, if d.live_medium { fg(th.faint) } else { fg(th.muted) });
            let cx = 2 + model_w + 1 + size_w + 2;
            if d.live_medium {
                c.text(row, cx, 0, &format!(" {} ", d.transport), fg(th.faint));
                c.text_ell(row, cx + chip_w + 1, 0, "Installer USB", fg(th.faint));
            } else {
                chip(c, rect_in(row, cx, 0, chip_w, 1), 0, &d.transport);
                let contents = if d.contents.is_empty() { "Empty".to_string() } else { d.contents.join(", ") };
                c.text_ell(row, cx + chip_w + 1, 0, &contents, fg(th.muted));
            }
            if !d.live_medium {
                c.hit(row, W_LIST, idx);
            }
        }
        let mut y = 2 + list_h as u16 + 1;
        if let Some(w) = self.windows() {
            let row = rect_in(a, 0, y, a.width, 1);
            if self.focus == Focus::Mode {
                c.text(row, 0, 0, "▌", fg(th.accent));
            }
            let labels = ["Install beside Windows", "Erase the whole disk"];
            segmented(c, row, 2, &labels, usize::from(!self.beside), self.focus == Focus::Mode, W_MODE);
            y += 1;
            if self.beside {
                let row = rect_in(a, 0, y, a.width, 1);
                match room(w) {
                    Room::Shrink(_) => {
                        if self.focus == Focus::Size {
                            c.text(row, 0, 0, "▌", fg(th.accent));
                        }
                        c.text(row, 2, 0, "Linux size", fg(th.muted));
                        let size = format!("{} GB", self.linux_gb);
                        stepper(c, row, 14, &size, self.focus == Focus::Size, W_SIZE);
                    }
                    Room::Free(b) => {
                        c.text_ell(row, 2, 0, &format!("Uses {} of free space.", fmt_size(b)), fg(th.muted));
                    }
                    Room::None(why) => {
                        let lines = wrap(&why, a.width.saturating_sub(4) as usize);
                        let n = lines.len().saturating_sub(1) as u16;
                        for (i, l) in lines.iter().enumerate() {
                            c.text(a, 2, y + i as u16, l, fg(th.amber));
                        }
                        y += n;
                    }
                }
            }
            y += 2;
        }
        let sel = self.selected();
        if let Some(d) = sel {
            self.draw_bar(c, rect_in(a, 0, y, a.width.min(76), 2), d.bytes, sh);
            y += 3;
            let windows = self.beside_windows();
            let shrink = match self.space() {
                Some(Ok(sp)) => sp.shrink,
                _ => None,
            };
            if let (Some(w), Some(sh)) = (windows, shrink) {
                let msg = format!("Windows shrinks from {} to {}.", fmt_size(w.windows.size), fmt_size(sh.to));
                c.text_ell(a, 2, y, &msg, fg(th.muted));
            } else if windows.is_none() && !d.contents.is_empty() {
                let msg = format!("Everything on {} ({}) will be erased.", d.model, fmt_size(d.bytes));
                c.text_ell(a, 2, y, &msg, fg(th.amber));
            }
            y += 2;
        } else {
            y += 5;
        }
        toggle(
            c,
            rect_in(a, 0, y, a.width, 1),
            self.encrypt,
            "Encrypt the disk",
            self.focus == Focus::Toggle,
            true,
            Hit { w: W_TOGGLE, i: 0 },
        );
        y += 1;
        if self.encrypt {
            let half = (a.width.saturating_sub(3) / 2).min(36);
            let pe = self.pass_error();
            let ce = self.confirm_error();
            draw_field(
                c,
                rect_in(a, 0, y, half, FIELD_ROWS),
                Field {
                    label: "Passphrase",
                    input: &self.pass,
                    placeholder: "",
                    focused: self.focus == Focus::Pass,
                    error: pe,
                    hint: None,
                    w: W_PASS,
                    i: 0,
                },
            );
            draw_field(
                c,
                rect_in(a, half + 3, y, half, FIELD_ROWS),
                Field {
                    label: "Confirm passphrase",
                    input: &self.confirm,
                    placeholder: "",
                    focused: self.focus == Focus::Confirm,
                    error: ce,
                    hint: None,
                    w: W_CONFIRM,
                    i: 0,
                },
            );
            y += FIELD_ROWS + 1;
        }
        let row = rect_in(a, 0, y, a.width, 1);
        if self.focus == Focus::Swap {
            c.text(row, 0, 0, "▌", fg(th.accent));
        }
        c.text(row, 2, 0, "Swap size", fg(th.muted));
        stepper(c, row, 14, &format!("{} GB", self.swap), self.focus == Focus::Swap, W_SWAP);
    }

    fn draw_bar(&self, c: &mut Canvas, a: Rect, disk_bytes: u64, sh: &Shared) {
        let th = *c.th;
        let subs = format!("{} @home @swap @snapshots @fspec", sh.subvolume());
        let disk_bytes = disk_bytes.max(1);
        // Every segment but the last, with its label, bytes and whether it is ours.
        let mut fixed: Vec<(String, u64, bool)> = Vec::new();
        match (self.beside_windows(), self.space()) {
            (Some(w), Some(Ok(sp))) => {
                let win = sp.shrink.as_ref().map_or(w.windows.size, |s| s.to);
                fixed.push((format!("Windows {}", fmt_size(win)), win, false));
                fixed.push(("boot 2 GB".into(), EFI_BYTES, true));
            }
            (Some(w), _) => fixed.push((format!("Windows {}", fmt_size(w.windows.size)), w.windows.size, false)),
            _ => fixed.push(("EFI 2 GB".into(), EFI_BYTES, true)),
        }
        let mut x = 0;
        let mut segs: Vec<(Rect, String, bool)> = Vec::new();
        for (label, bytes, ours) in fixed {
            let min = label.width() as u16 + 2;
            let w = ((a.width as u64 * bytes / disk_bytes) as u16).clamp(min, (a.width / 2).max(min));
            segs.push((Rect { x: a.x + x, y: a.y + 1, width: w, height: 1 }, label, ours));
            x += w + 1;
        }
        let rest_x = x;
        let beside_only = self.beside_windows().is_some() && !matches!(self.space(), Some(Ok(_)));
        if !beside_only {
            segs.push((
                Rect { x: a.x + rest_x, y: a.y + 1, width: a.width.saturating_sub(rest_x), height: 1 },
                subs,
                true,
            ));
        }
        if self.encrypt && !beside_only {
            c.text(a, rest_x, 0, "LUKS2 ", fg(th.accent));
            for x in rest_x + 6..a.width {
                c.text(a, x, 0, "─", fg(th.line));
            }
        }
        let n = segs.len();
        for (i, (r, label, ours)) in segs.into_iter().enumerate() {
            let text = if !ours {
                fg(th.faint)
            } else if i + 1 == n {
                fg(th.muted)
            } else {
                fg(th.ink)
            };
            if th.truecolor {
                let bg = if ours && i + 1 < n { th.sel } else { th.surface };
                c.fill(r, Style::new().bg(bg));
                c.text_ell(r, 1, 0, &label, text);
            } else {
                c.text_ell(r, 0, 0, &format!("[{label}]"), text);
            }
        }
    }
}
