use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use unicode_width::UnicodeWidthStr;

use super::anim::{self, FRAME, SLIDE, SLIDE_COLS};
use super::screens::modal::{Modal, Out};
use super::screens::review::Summary;
use super::screens::{
    Exit, Flow, Hints, Shared, Step, account::AccountScreen, disk::DiskScreen, flow, host::HostScreen,
    install::InstallScreen, keys::KeysScreen, language::Language, preset::PresetScreen, review::Review, source::Source,
    system::System, welcome::Welcome,
};
use super::theme::{Theme, fg};
use super::widgets::spec::SpecPane;
use super::widgets::{Canvas, Hit, W_SPEC, W_TRAIL, at, fmt_size};
use crate::backend::Backend;
use crate::model::{Desktop, Draft, HostSummary, Preset, SystemChoice};

pub const MIN_W: u16 = 80;
pub const MIN_H: u16 = 24;
const SPEC_MIN_W: u16 = 110;

pub enum Input {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Resize(u16, u16),
    Tick,
}

type Cloned = Result<Vec<HostSummary>, String>;
type Fetched = Result<crate::keys::Key, String>;

pub struct App {
    pub sh: Shared,
    theme: Theme,
    motion: bool,
    epoch: Instant,
    step: Step,
    reached: HashSet<Step>,
    trans: Option<(Instant, i32)>,
    welcome: Welcome,
    lang: Language,
    source: Source,
    host: HostScreen,
    keys: KeysScreen,
    disk: DiskScreen,
    system: System,
    preset: PresetScreen,
    account: AccountScreen,
    review: Review,
    install: InstallScreen,
    spec: SpecPane,
    last_draft: Option<Draft>,
    spec_overlay: bool,
    modal: Option<Modal>,
    note: Option<&'static str>,
    hits: Vec<(Rect, Hit)>,
    size: (u16, u16),
    clone_tx: Sender<Cloned>,
    clone_rx: Receiver<Cloned>,
    key_tx: Sender<Fetched>,
    key_rx: Receiver<Fetched>,
}

impl App {
    pub fn new(
        be: Arc<dyn Backend>,
        theme: Theme,
        motion: bool,
        checkout: PathBuf,
        size: (u16, u16),
        now: Instant,
    ) -> App {
        let sh = Shared {
            firmware: be.firmware(),
            be,
            restore: false,
            distro: None,
            preset: Preset::Minimal,
            desktop: None::<Desktop>,
            host: None,
            url: String::new(),
            auth: crate::model::GitAuth::None,
            checkout,
        };
        let (clone_tx, clone_rx) = mpsc::channel();
        let (key_tx, key_rx) = mpsc::channel();
        let mut app = App {
            lang: Language::new(&sh),
            sh,
            theme,
            motion: motion && theme.truecolor,
            epoch: now,
            step: Step::Welcome,
            reached: HashSet::new(),
            trans: None,
            welcome: Welcome::default(),
            source: Source::new(),
            host: HostScreen::new(),
            keys: KeysScreen::new(),
            disk: DiskScreen::new(),
            system: System::new(),
            preset: PresetScreen::new(),
            account: AccountScreen::new(),
            review: Review::default(),
            install: InstallScreen::new(),
            spec: SpecPane::default(),
            last_draft: None,
            spec_overlay: false,
            modal: None,
            note: None,
            hits: Vec::new(),
            size,
            clone_tx,
            clone_rx,
            key_tx,
            key_rx,
        };
        app.sync(now);
        app
    }

    pub fn step(&self) -> Step {
        self.step
    }

    pub fn install_outcome(&self) -> &super::screens::install::Outcome {
        &self.install.outcome
    }

    pub fn modal_open(&self) -> bool {
        self.modal.is_some()
    }

    pub fn busy(&self) -> bool {
        self.source.cloning.is_some() || self.keys.fetching.is_some() || self.install.running()
    }

    pub fn next_wake(&self, now: Instant) -> Option<Duration> {
        let sliding = self.trans.is_some_and(|(t, _)| now.saturating_duration_since(t) < SLIDE);
        if sliding || self.spec.animating(now) {
            Some(FRAME)
        } else if self.busy() {
            Some(anim::until_next_spin(self.epoch, now))
        } else {
            None
        }
    }

    fn wide(&self) -> bool {
        self.size.0 >= SPEC_MIN_W
    }

    fn has_spec(&self) -> bool {
        !matches!(self.step, Step::Welcome | Step::Install)
    }

    fn order(&self, s: Step) -> i32 {
        flow(self.sh.restore).iter().position(|x| *x == s).map_or(-1, |p| p as i32)
    }

    fn reached(&self, s: Step) -> bool {
        self.reached.contains(&s)
    }

    pub fn draft(&self) -> Draft {
        let sh = &self.sh;
        let system = if sh.restore {
            sh.host.as_ref().filter(|_| self.reached(Step::Host)).map(|h| SystemChoice::Restore {
                url: sh.url.clone(),
                auth: sh.auth.clone(),
                checkout: sh.checkout.clone(),
                host: h.name.clone(),
            })
        } else if self.reached(Step::System) {
            sh.distro.map(|d| SystemChoice::Fresh { distro: d.id, preset: sh.preset, desktop: sh.desktop })
        } else {
            None
        };
        Draft {
            locale: self.reached(Step::Language).then(|| self.lang.value()),
            disk: if self.reached(Step::Disk) { self.disk.value() } else { None },
            system,
            account: self.reached(Step::Account).then(|| self.account.value(sh)),
            secure_boot: self.review.secure_boot && sh.firmware.efi && sh.firmware.setup_mode,
            keys: if sh.restore && self.reached(Step::Keys) { self.keys.value() } else { Vec::new() },
        }
    }

    fn sync(&mut self, now: Instant) {
        if let (false, true, Some(d)) = (self.sh.restore, self.reached(Step::System), self.system.current()) {
            if self.sh.distro.map(|x| x.id) != Some(d.id) {
                self.sh.distro = Some(d);
                self.preset.set_distro(d);
            }
            (self.sh.preset, self.sh.desktop) = self.preset.current();
        }
        let d = self.draft();
        if self.last_draft.as_ref() != Some(&d) {
            let text = self.sh.be.preview(&d);
            self.spec.update(&text, now);
            self.last_draft = Some(d);
        }
    }

    fn summary(&self) -> Summary {
        let sh = &self.sh;
        let disk = self.disk.selected();
        let d = self.disk.value();
        let disk_line = match (disk, &d) {
            (Some(i), Some(v)) => {
                let enc = if v.encrypt.is_some() { " · encrypted" } else { "" };
                format!("{} ({}){enc} · {} GB swap", i.model, fmt_size(i.bytes), v.swap_gib)
            }
            _ => String::new(),
        };
        let system = if sh.restore {
            format!("{} from {}", sh.host.as_ref().map_or("", |h| h.name.as_str()), sh.url)
        } else {
            let (preset, desktop) = (sh.preset, sh.desktop);
            let p = super::screens::preset::title(preset);
            let desk = desktop.map(|d| format!(" ({})", d.name())).unwrap_or_default();
            format!("{} · {p}{desk}", sh.distro.map_or("", |d| d.name))
        };
        let acct = self.account.value(sh);
        let mut groups = vec![
            ("Language", self.lang.describe()),
            ("Disk", disk_line),
            ("System", system),
            ("Account", format!("{} ({}) on {}", acct.full_name, acct.name, acct.hostname)),
        ];
        if sh.restore {
            let keys = self.keys.value();
            let line = if keys.is_empty() {
                "None; secrets no key opens are left out".to_string()
            } else {
                let names: Vec<String> =
                    keys.iter().map(|k| if k.generated { "a new age key".into() } else { k.label() }).collect();
                names.join(", ")
            };
            groups.push(("Keys", line));
        }
        Summary {
            groups,
            device: disk.map(|i| i.path.clone()).unwrap_or_default(),
            model: disk.map(|i| i.model.clone()).unwrap_or_default(),
            distro: sh.distro,
            windows: d.as_ref().and_then(|v| v.windows.as_ref()).map(windows_summary),
        }
    }

    fn captures_text(&self) -> bool {
        match self.step {
            Step::Welcome | Step::Install | Step::Host | Step::System | Step::Preset | Step::Review => false,
            Step::Keys => self.keys.captures_text(),
            Step::Language => self.lang.captures_text(),
            Step::Source => self.source.captures_text(),
            Step::Disk => self.disk.captures_text(),
            Step::Account => self.account.captures_text(),
        }
    }

    // ---- input ----

    pub fn handle(&mut self, ev: Input, now: Instant) -> Option<Exit> {
        self.poll(now);
        let exit = match ev {
            Input::Key(k) => self.on_key(k, now),
            Input::Mouse(m) => self.on_mouse(m, now),
            Input::Resize(w, h) => {
                self.size = (w, h);
                None
            }
            Input::Tick => None,
        };
        self.sync(now);
        exit
    }

    fn poll(&mut self, now: Instant) {
        while let Ok(r) = self.clone_rx.try_recv() {
            match r {
                Ok(hosts) => {
                    self.source.done();
                    self.host.set_hosts(hosts);
                    self.go(Step::Host, now);
                }
                Err(e) => self.source.fail(&e),
            }
        }
        while let Ok(r) = self.key_rx.try_recv() {
            self.keys.fetched(r, &self.sh);
        }
        self.install.poll(now);
    }

    fn on_key(&mut self, k: KeyEvent, now: Instant) -> Option<Exit> {
        self.note = None;
        if k.kind == ratatui::crossterm::event::KeyEventKind::Release {
            return None;
        }
        if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
            if self.install.running() {
                self.note = Some("The install is running and cannot be stopped.");
            } else if !matches!(self.modal, Some(Modal::Quit { .. })) {
                self.modal = Some(Modal::quit());
            }
            return None;
        }
        if let Some(m) = &mut self.modal {
            let out = m.key(&k);
            return self.modal_out(out, now);
        }
        if self.spec_overlay {
            match k.code {
                KeyCode::Esc | KeyCode::Char('s' | 'q') => self.spec_overlay = false,
                KeyCode::Up | KeyCode::Char('k') => self.spec.wheel(-1),
                KeyCode::Down | KeyCode::Char('j') => self.spec.wheel(1),
                KeyCode::PageUp => self.spec.wheel(-10),
                KeyCode::PageDown => self.spec.wheel(10),
                _ => {}
            }
            return None;
        }
        if !self.captures_text() && !self.install.running() {
            match k.code {
                KeyCode::Char('?') => {
                    self.modal = Some(Modal::Help);
                    return None;
                }
                KeyCode::Char('s') if !self.wide() && self.has_spec() => {
                    self.spec_overlay = true;
                    return None;
                }
                _ => {}
            }
        }
        let f = match self.step {
            Step::Welcome => self.welcome.key(&k, &mut self.sh),
            Step::Language => self.lang.key(&k, &mut self.sh),
            Step::Source => self.source.key(&k, &mut self.sh),
            Step::Host => self.host.key(&k, &mut self.sh),
            Step::Keys => self.keys.key(&k, &mut self.sh),
            Step::Disk => self.disk.key(&k, &mut self.sh),
            Step::System => self.system.key(&k, &mut self.sh),
            Step::Preset => self.preset.key(&k, &mut self.sh),
            Step::Account => self.account.key(&k, &mut self.sh),
            Step::Review => self.review.key(&k, &mut self.sh),
            Step::Install => self.install.key(&k),
        };
        self.apply(f, now)
    }

    fn on_mouse(&mut self, m: MouseEvent, now: Instant) -> Option<Exit> {
        let hit = at(&self.hits, m.column, m.row);
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let hit = hit?;
                if let Some(modal) = &mut self.modal {
                    let out = modal.click(hit);
                    return self.modal_out(out, now);
                }
                if self.spec_overlay {
                    return None;
                }
                let f = match (hit.w, self.step) {
                    (W_TRAIL, _) => {
                        let target = flow(self.sh.restore).get(hit.i).copied();
                        let cur = self.order(self.step);
                        match target {
                            Some(t) if (hit.i as i32) < cur && !self.install.running() && t != Step::Install => {
                                Flow::Jump(t)
                            }
                            _ => Flow::Stay,
                        }
                    }
                    (W_SPEC, _) => Flow::Stay,
                    (_, Step::Welcome) => self.welcome.click(hit),
                    (_, Step::Language) => self.lang.click(hit, &self.sh),
                    (_, Step::Source) => self.source.click(hit),
                    (_, Step::Host) => self.host.click(hit, &mut self.sh),
                    (_, Step::Keys) => self.keys.click(hit),
                    (_, Step::Disk) => self.disk.click(hit),
                    (_, Step::System) => self.system.click(hit),
                    (_, Step::Preset) => self.preset.click(hit),
                    (_, Step::Account) => self.account.click(hit),
                    (_, Step::Review) => self.review.click(hit, &self.sh),
                    (_, Step::Install) => self.install.click(hit),
                };
                self.apply(f, now)
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let d = if m.kind == MouseEventKind::ScrollUp { -3 } else { 3 };
                if self.modal.is_some() {
                    return None;
                }
                let w = hit.map_or(0, |h| h.w);
                if w == W_SPEC {
                    self.spec.wheel(d);
                    return None;
                }
                match self.step {
                    Step::Language => self.lang.wheel(w, d),
                    Step::Host => self.host.wheel(d),
                    Step::Disk => self.disk.wheel(w, d),
                    Step::System => self.system.wheel(w, d),
                    _ => {}
                }
                None
            }
            _ => None,
        }
    }

    fn modal_out(&mut self, out: Out, now: Instant) -> Option<Exit> {
        match out {
            Out::Stay => None,
            Out::Close => {
                self.modal = None;
                None
            }
            Out::Quit => Some(Exit::Quit),
            Out::Install => {
                self.modal = None;
                self.start_install(now);
                None
            }
        }
    }

    fn apply(&mut self, f: Flow, now: Instant) -> Option<Exit> {
        match f {
            Flow::Stay => {}
            Flow::Next => self.advance(now),
            Flow::Back => self.retreat(now),
            Flow::Jump(s) => self.go(s, now),
            Flow::Modal(m) => self.modal = Some(m),
            Flow::Clone => self.start_clone(),
            Flow::FetchKey => self.start_fetch(),
            Flow::Install => self.start_install(now),
            Flow::Exit(e) => return Some(e),
        }
        None
    }

    fn advance(&mut self, now: Instant) {
        if self.step == Step::Welcome {
            self.sh.restore = self.welcome.sel == 1;
            self.reached.clear();
            self.go(Step::Language, now);
            return;
        }
        let f = flow(self.sh.restore);
        if let Some(next) = f.iter().position(|s| *s == self.step).and_then(|p| f.get(p + 1)) {
            self.go(*next, now);
        }
    }

    fn retreat(&mut self, now: Instant) {
        if self.step == Step::Welcome {
            self.modal = Some(Modal::quit());
            return;
        }
        let f = flow(self.sh.restore);
        match f.iter().position(|s| *s == self.step) {
            Some(0) => self.go(Step::Welcome, now),
            Some(p) if self.step != Step::Install => self.go(f[p - 1], now),
            _ => {}
        }
    }

    fn go(&mut self, step: Step, now: Instant) {
        let forward = self.order(step) >= self.order(self.step);
        let from = self.step;
        self.step = step;
        self.reached.insert(step);
        match step {
            Step::Language => self.lang.apply_keymap(&self.sh),
            Step::Disk => self.disk.reload(&self.sh),
            Step::Keys => self.keys.enter(&self.sh),
            Step::Account => self.account.enter(&self.sh),
            Step::Review => {
                // A failed install beside Windows can leave C: shrunk, so the retry plans from
                // the disk as it is now.
                if from == Step::Install {
                    self.disk.reload(&self.sh);
                }
                let s = self.summary();
                self.review.enter(s);
            }
            _ => {}
        }
        if self.motion {
            self.trans = Some((now, if forward { 1 } else { -1 }));
        }
        self.sync(now);
    }

    fn start_clone(&mut self) {
        let (url, auth) = self.source.request();
        self.sh.url = url.clone();
        self.sh.auth = auth.clone();
        let be = self.sh.be.clone();
        let tx = self.clone_tx.clone();
        let into = self.sh.checkout.clone();
        std::thread::spawn(move || {
            let r = be.clone_spec(&url, &auth, &into).map_err(|e| format!("{e:#}"));
            let _ = tx.send(r);
        });
    }

    fn start_fetch(&mut self) {
        let Some(source) = self.keys.request() else { return };
        let be = self.sh.be.clone();
        let tx = self.key_tx.clone();
        std::thread::spawn(move || {
            let r = be.fetch_key(&source).map_err(|e| format!("{e:#}"));
            let _ = tx.send(r);
        });
    }

    fn start_install(&mut self, now: Instant) {
        let Some(choices) = self.draft().complete() else { return };
        let (tx, rx) = mpsc::channel();
        let name = self.sh.distro.map_or("Your system", |d| d.name);
        let generated = choices.keys.iter().filter(|k| k.generated);
        let notes = generated.flat_map(|k| crate::keys::rekey_steps(&choices.account.hostname, k)).collect();
        self.install.start(rx, now, name, notes);
        let be = self.sh.be.clone();
        std::thread::spawn(move || be.install(&choices, tx));
        self.go(Step::Install, now);
    }

    // ---- drawing ----

    fn hints(&self) -> Hints {
        let mut h = match self.step {
            Step::Welcome => self.welcome.hints(),
            Step::Language => self.lang.hints(),
            Step::Source => self.source.hints(),
            Step::Host => self.host.hints(),
            Step::Keys => self.keys.hints(),
            Step::Disk => self.disk.hints(),
            Step::System => self.system.hints(),
            Step::Preset => self.preset.hints(),
            Step::Account => self.account.hints(),
            Step::Review => self.review.hints(),
            Step::Install => self.install.hints(),
        };
        if !self.wide() && self.has_spec() && !self.captures_text() {
            h.push(("s", "Spec"));
        }
        h
    }

    pub fn render(&mut self, buf: &mut Buffer, now: Instant) {
        let area = buf.area;
        self.size = (area.width, area.height);
        self.hits.clear();
        let th = self.theme;
        buf.set_style(area, th.base());
        if area.width < MIN_W || area.height < MIN_H {
            let msg = "Make the window at least 80 by 24.";
            let x = area.x + area.width.saturating_sub(msg.width() as u16) / 2;
            buf.set_stringn(x, area.y + area.height / 2, msg, area.width as usize, fg(th.muted));
            return;
        }
        let mut hits = std::mem::take(&mut self.hits);
        let (w, h) = (area.width, area.height);
        let show_pane = self.has_spec() && (self.wide() || self.spec_overlay);
        let pane_w = w * 40 / 100;
        let (content, pane) = if self.wide() && self.has_spec() {
            let px = w - 2 - pane_w;
            (Rect::new(2, 3, px.saturating_sub(5), h - 4), Rect::new(px, 3, pane_w, h - 4))
        } else {
            (Rect::new(2, 3, w - 4, h - 4), Rect::new(2, 2, w - 4, h - 3))
        };
        {
            let mut c = Canvas { buf: &mut *buf, th: &th, hits: &mut hits, now, epoch: self.epoch };
            self.draw_chrome(&mut c, area);
            if self.spec_overlay && !self.wide() {
                let cur = current_lines(self.step);
                self.spec.draw(&mut c, pane, &cur, self.motion);
            } else {
                if show_pane {
                    let cur = current_lines(self.step);
                    self.spec.draw(&mut c, pane, &cur, self.motion);
                }
                if let Some((start, dir)) = self.trans {
                    let p = anim::progress(start, now, SLIDE);
                    if p >= 1.0 {
                        self.trans = None;
                    } else {
                        let mut scratch = Buffer::empty(content);
                        scratch.set_style(content, th.base());
                        let mut sc = Canvas { buf: &mut scratch, th: &th, hits: &mut *c.hits, now, epoch: self.epoch };
                        self.draw_screen(&mut sc, content);
                        blit(c.buf, &scratch, content, dir, anim::ease_out(p), &th);
                    }
                }
                if self.trans.is_none() {
                    self.draw_screen(&mut c, content);
                }
            }
        }
        if let Some(m) = &mut self.modal {
            dim(buf, &th);
            hits.clear();
            let mut c = Canvas { buf: &mut *buf, th: &th, hits: &mut hits, now, epoch: self.epoch };
            m.render(&mut c, area);
        }
        self.hits = hits;
    }

    fn draw_screen(&mut self, c: &mut Canvas, a: Rect) {
        match self.step {
            Step::Welcome => self.welcome.render(c, a, &self.sh),
            Step::Language => self.lang.render(c, a, &self.sh),
            Step::Source => self.source.render(c, a, &self.sh),
            Step::Host => self.host.render(c, a, &self.sh),
            Step::Keys => self.keys.render(c, a, &self.sh),
            Step::Disk => self.disk.render(c, a, &self.sh),
            Step::System => self.system.render(c, a, &self.sh),
            Step::Preset => self.preset.render(c, a, &self.sh),
            Step::Account => self.account.render(c, a, &self.sh),
            Step::Review => self.review.render(c, a, &self.sh),
            Step::Install => self.install.render(c, a),
        }
    }

    fn draw_chrome(&self, c: &mut Canvas, area: Rect) {
        let th = *c.th;
        let w = area.width;
        let names: Vec<&str> = flow(self.sh.restore).iter().map(|s| s.title()).collect();
        let cur = flow(self.sh.restore).iter().position(|s| *s == self.step);
        let trail_w: usize = names.iter().map(|n| n.width()).sum::<usize>() + 3 * (names.len() - 1);
        let mut x = w.saturating_sub(2 + trail_w as u16);
        for (i, n) in names.iter().enumerate() {
            let st = match cur {
                Some(p) if i < p => fg(th.ink),
                Some(p) if i == p => Style::new().fg(th.accent).add_modifier(Modifier::BOLD),
                _ => fg(th.faint),
            };
            let nw = n.width() as u16;
            c.text(area, x, 0, n, st);
            if cur.is_some_and(|p| i < p) && self.step != Step::Install {
                c.hit(Rect::new(area.x + x, area.y, nw, 1), W_TRAIL, i);
            }
            x += nw;
            if i + 1 < names.len() {
                c.text(area, x, 0, " · ", fg(th.faint));
                x += 3;
            }
        }
        let brand = x_of_trail(w, trail_w as u16);
        // The restore trail fills an 80 column screen.
        if brand >= 2 + 10 + 2 {
            c.text(area, 2, 0, "FormalSpec", Style::new().fg(th.ink).add_modifier(Modifier::BOLD));
        }
        if brand >= 2 + 10 + 10 + 2 {
            c.text(area, 12, 0, " installer", fg(th.muted));
        }
        for i in 2..w.saturating_sub(2) {
            c.text(area, i, 1, "─", fg(th.line));
        }
        // Footer.
        let y = area.height - 1;
        let mut x = 2;
        for (k, l) in self.hints() {
            if !k.is_empty() {
                x += c.text(area, x, y, k, fg(th.ink));
                x += c.text(area, x, y, " ", fg(th.muted));
            }
            x += c.text(area, x, y, l, fg(th.muted));
            x += 2;
        }
        if let Some(n) = self.note {
            let nw = n.width() as u16;
            c.text(area, w.saturating_sub(nw + 2), y, n, fg(th.amber));
        }
    }
}

fn x_of_trail(w: u16, trail_w: u16) -> u16 {
    w.saturating_sub(2 + trail_w)
}

fn current_lines(step: Step) -> impl Fn(&str) -> bool {
    move |l: &str| match step {
        Step::Language => ["locale", "keymap", "timezone"].iter().any(|k| l.contains(k)),
        Step::System | Step::Preset => l.contains("modules") || l.starts_with("load("),
        Step::Account => l.contains("users") || l.trim_start().starts_with("name"),
        _ => false,
    }
}

fn blit(dst: &mut Buffer, src: &Buffer, a: Rect, dir: i32, e: f32, th: &Theme) {
    let shift = (SLIDE_COLS as f32 * (1.0 - e)).round() as i32 * dir;
    for y in a.top()..a.bottom() {
        for x in a.left()..a.right() {
            let dx = x as i32 + shift;
            if dx < a.left() as i32 || dx >= a.right() as i32 {
                continue;
            }
            let mut cell = src[(x, y)].clone();
            if cell.symbol() != " " {
                let fin = if cell.fg == ratatui::style::Color::Reset { th.ink } else { cell.fg };
                cell.fg = anim::lerp_color(th.faint, fin, e);
            }
            dst[(dx as u16, y)] = cell;
        }
    }
}

fn dim(buf: &mut Buffer, th: &Theme) {
    for cell in buf.content.iter_mut() {
        if th.truecolor {
            let fin = if cell.fg == ratatui::style::Color::Reset { th.ink } else { cell.fg };
            cell.fg = anim::lerp_color(fin, th.bg, 0.6);
        } else {
            cell.modifier.insert(Modifier::DIM);
        }
    }
}

/// What the review and the confirm modal say happens to Windows.
fn windows_summary(b: &fspec_switch::BesideWindows) -> super::screens::review::Windows {
    let device = &b.disk.disk;
    let (review, confirm) = match &b.space.shrink {
        Some(sh) => (
            format!("Windows shrinks by {} and stays in the boot menu.", fmt_size(b.disk.windows.size - sh.to)),
            format!("Windows on {device} shrinks to {}.", fmt_size(sh.to)),
        ),
        None => (
            "Windows is untouched and stays in the boot menu.".to_string(),
            format!("Linux goes into free space on {device}."),
        ),
    };
    super::screens::review::Windows { review, confirm, bitlocker: b.disk.bitlocker() }
}
