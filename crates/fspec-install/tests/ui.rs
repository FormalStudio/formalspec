use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fspec_install::backend::Backend;
use fspec_install::demo::DemoBackend;
use fspec_install::model::*;
use fspec_install::ui::app::{App, Input};
use fspec_install::ui::screens::install::Outcome;
use fspec_install::ui::screens::{Exit, Step};
use fspec_install::ui::theme::Theme;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use unicode_width::UnicodeWidthStr;

/// Plays the demo install up to stage 3, then waits for `release` (or fails there).
struct Scripted {
    inner: DemoBackend,
    gate: Mutex<Option<Receiver<()>>>,
    fail: bool,
    loaded: Mutex<Vec<String>>,
}

impl Backend for Scripted {
    fn disks(&self) -> anyhow::Result<Vec<DiskInfo>> {
        self.inner.disks()
    }
    fn firmware(&self) -> Firmware {
        self.inner.firmware()
    }
    fn locales(&self) -> Vec<(String, String)> {
        self.inner.locales()
    }
    fn keymaps(&self) -> Vec<(String, String)> {
        self.inner.keymaps()
    }
    fn timezones(&self) -> Vec<String> {
        self.inner.timezones()
    }
    fn set_keymap(&self, k: &str) -> anyhow::Result<()> {
        self.loaded.lock().unwrap().push(k.to_string());
        self.inner.set_keymap(k)
    }
    fn clone_spec(&self, u: &str, a: &GitAuth, i: &Path) -> anyhow::Result<Vec<HostSummary>> {
        self.inner.clone_spec(u, a, i)
    }
    fn preview(&self, d: &Draft) -> String {
        self.inner.preview(d)
    }
    fn install(&self, _c: &Choices, tx: Sender<Progress>) {
        let titles = ["Partition the disk", "Format the partitions", "Set up LUKS2"];
        for (index, t) in titles.iter().enumerate() {
            tx.send(Progress::Stage { index, total: 12, title: t.to_string() }).unwrap();
            tx.send(Progress::Output(format!("sgdisk --new={}:0:0 /dev/nvme0n1", index + 1))).unwrap();
            tx.send(Progress::Output("The operation has completed successfully.".into())).unwrap();
        }
        if self.fail {
            tx.send(Progress::Failed {
                stage: "Set up LUKS2".into(),
                error: "cryptsetup luksFormat exited with status 1: Device /dev/nvme0n1p2 is still in use.".into(),
                log: PathBuf::from("/var/log/fspec-install.log"),
            })
            .unwrap();
            return;
        }
        let gate = self.gate.lock().unwrap().take().unwrap();
        let _ = gate.recv();
        tx.send(Progress::Done).unwrap();
    }
}

fn scripted(fail: bool) -> (Arc<Scripted>, Sender<()>) {
    let (tx, rx) = channel();
    (
        Arc::new(Scripted {
            inner: DemoBackend::instant(),
            gate: Mutex::new(Some(rx)),
            fail,
            loaded: Mutex::default(),
        }),
        tx,
    )
}

struct Rig {
    app: App,
    now: Instant,
    t0: Instant,
}

impl Rig {
    fn new(be: Arc<dyn Backend>, size: (u16, u16), motion: bool) -> Rig {
        let t0 = Instant::now();
        let app = App::new(be, Theme::truecolor(), motion, PathBuf::from("/run/fspec-install/clone"), size, t0);
        Rig { app, now: t0, t0 }
    }

    fn demo() -> Rig {
        Rig::new(Arc::new(DemoBackend::instant()), (120, 40), false)
    }

    fn send(&mut self, i: Input) -> Option<Exit> {
        self.app.handle(i, self.now)
    }

    fn press(&mut self, code: KeyCode) -> Option<Exit> {
        self.send(Input::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn ctrl_c(&mut self) {
        self.send(Input::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));
    }

    fn typ(&mut self, s: &str) {
        for c in s.chars() {
            self.press(KeyCode::Char(c));
        }
    }

    fn click(&mut self, x: u16, y: u16) {
        self.send(Input::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        }));
    }

    fn settle(&mut self) {
        for _ in 0..2000 {
            self.send(Input::Tick);
            if !self.app.busy() {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!("background work did not finish");
    }

    /// Ticks until the screen at `size` shows `needle`.
    fn wait_for(&mut self, needle: &str) {
        for _ in 0..2000 {
            self.send(Input::Tick);
            if self.text((120, 40)).contains(needle) {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!("never saw {needle:?}:\n{}", self.text((120, 40)));
    }

    fn text(&mut self, size: (u16, u16)) -> String {
        self.text_at(size, self.now + Duration::from_secs(5))
    }

    fn text_at(&mut self, size: (u16, u16), at: Instant) -> String {
        let mut term = Terminal::new(TestBackend::new(size.0, size.1)).unwrap();
        term.draw(|f| self.app.render(f.buffer_mut(), at)).unwrap();
        dump(term.backend().buffer())
    }

    fn account(&mut self) {
        self.typ("Kyan de Sutter");
        self.press(KeyCode::Tab);
        self.press(KeyCode::Tab);
        self.press(KeyCode::Tab);
        self.typ("correct horse");
        self.press(KeyCode::Tab);
        self.typ("correct horse");
        self.press(KeyCode::Enter);
    }

    fn erase_and_install(&mut self, device: &str) {
        self.press(KeyCode::Enter);
        assert!(self.app.modal_open());
        self.typ(device);
        self.press(KeyCode::Enter);
    }

    /// Welcome through the new-system flow until `step` is current.
    fn to(&mut self, step: Step) {
        let order =
            [Step::Welcome, Step::Language, Step::Disk, Step::System, Step::Preset, Step::Account, Step::Review];
        for s in order.iter().skip(1) {
            if self.app.step() == step {
                return;
            }
            if *s == Step::Review {
                self.account();
            } else {
                self.press(KeyCode::Enter);
            }
        }
        assert_eq!(self.app.step(), step);
    }
}

fn dump(buf: &Buffer) -> String {
    let a = buf.area;
    let mut out = String::new();
    for y in 0..a.height {
        let mut line = String::new();
        let mut skip = 0;
        for x in 0..a.width {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            let sym = buf[(x, y)].symbol();
            line.push_str(sym);
            skip = sym.width().saturating_sub(1);
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

fn golden(name: &str, text: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/screens").join(format!("{name}.txt"));
    if std::env::var("UPDATE_SCREENS").is_ok_and(|v| v == "1") {
        std::fs::write(&path, text).unwrap();
        return;
    }
    let want = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("missing {}; run with UPDATE_SCREENS=1", path.display()));
    assert_eq!(want, text, "{name} differs; run with UPDATE_SCREENS=1 and review the file");
}

const SIZES: [(&str, (u16, u16)); 2] = [("120x40", (120, 40)), ("80x24", (80, 24))];

fn shots(rig: &mut Rig, name: &str) {
    for (tag, size) in SIZES {
        let t = rig.text(size);
        golden(&format!("{name}-{tag}"), &t);
    }
}

// ---- flows ----

#[test]
fn new_system_flow_reaches_install_done() {
    let mut r = Rig::demo();
    assert_eq!(r.app.step(), Step::Welcome);
    r.press(KeyCode::Enter);
    assert_eq!(r.app.step(), Step::Language);
    r.press(KeyCode::Enter);
    assert_eq!(r.app.step(), Step::Disk);
    r.press(KeyCode::Enter);
    assert_eq!(r.app.step(), Step::System);
    r.press(KeyCode::Down);
    r.press(KeyCode::Up);
    r.press(KeyCode::Enter);
    assert_eq!(r.app.step(), Step::Preset);
    r.press(KeyCode::Enter);
    assert_eq!(r.app.step(), Step::Account);
    r.account();
    assert_eq!(r.app.step(), Step::Review);
    let c = r.app.draft().complete().expect("complete draft at review");
    assert_eq!(c.account.name, "kyan");
    assert_eq!(c.account.hostname, "kyan-fedora");
    assert_eq!(c.disk.device, "/dev/nvme0n1");
    assert_eq!(c.locale.keymap, "us");
    assert_eq!(c.system, SystemChoice::Fresh { distro: "fedora", preset: Preset::Minimal, desktop: None });
    r.erase_and_install("/dev/nvme0n1");
    assert_eq!(r.app.step(), Step::Install);
    r.settle();
    assert!(matches!(r.app.install_outcome(), Outcome::Done));
    assert!(r.text((120, 40)).contains("Fedora is installed."));
    assert_eq!(r.press(KeyCode::Enter), Some(Exit::Restart));
}

fn last_without_uefi() -> &'static str {
    fspec_install::catalog::DISTROS.iter().rev().find(|d| !d.needs_efi).unwrap().id
}

#[test]
fn a_bios_machine_cannot_pick_the_distros_that_need_uefi() {
    let mut r = Rig::new(Arc::new(DemoBackend::instant().bios()), (120, 40), false);
    for _ in 0..3 {
        r.press(KeyCode::Enter);
    }
    assert_eq!(r.app.step(), Step::System);
    r.press(KeyCode::End);
    r.press(KeyCode::Enter);
    let Some(SystemChoice::Fresh { distro, .. }) = r.app.draft().system else { panic!("no system chosen") };
    assert_eq!(distro, last_without_uefi());
    r.press(KeyCode::Esc);
    let screen = r.text((120, 40));
    let y = screen.lines().position(|l| l.contains("AerynOS")).unwrap() as u16;
    r.click(4, y);
    r.click(4, y);
    assert_eq!(r.app.step(), Step::System);
    let Some(SystemChoice::Fresh { distro, .. }) = r.app.draft().system else { panic!("no system chosen") };
    assert_eq!(distro, last_without_uefi());
}

#[test]
fn install_done_can_open_a_shell() {
    let mut r = Rig::demo();
    r.to(Step::Review);
    r.erase_and_install("/dev/nvme0n1");
    r.settle();
    r.press(KeyCode::Right);
    assert_eq!(r.press(KeyCode::Enter), Some(Exit::Shell));
}

#[test]
fn restore_flow_reaches_install_done() {
    let mut r = Rig::demo();
    r.press(KeyCode::Right);
    r.press(KeyCode::Enter);
    r.press(KeyCode::Enter);
    assert_eq!(r.app.step(), Step::Source);
    r.typ("git@github.com:me/spec.git");
    r.press(KeyCode::Enter);
    r.settle();
    assert_eq!(r.app.step(), Step::Host);
    r.press(KeyCode::Down);
    r.press(KeyCode::Enter);
    assert_eq!(r.app.step(), Step::Disk);
    r.press(KeyCode::Enter);
    assert_eq!(r.app.step(), Step::Account);
    r.typ("correct horse");
    r.press(KeyCode::Tab);
    r.typ("correct horse");
    r.press(KeyCode::Enter);
    assert_eq!(r.app.step(), Step::Review);
    let c = r.app.draft().complete().expect("complete draft");
    assert!(matches!(&c.system, SystemChoice::Restore { host, .. } if host == "framework"));
    assert_eq!(c.account.name, "kyan");
    r.erase_and_install("/dev/nvme0n1");
    r.settle();
    assert!(matches!(r.app.install_outcome(), Outcome::Done));
}

#[test]
fn failed_clone_shows_how_to_fix_it() {
    let mut r = Rig::demo();
    r.press(KeyCode::Right);
    r.press(KeyCode::Enter);
    r.press(KeyCode::Enter);
    r.typ("https://example.com/fail.git");
    r.press(KeyCode::Enter);
    r.settle();
    assert_eq!(r.app.step(), Step::Source);
    let t = r.text((120, 40));
    assert!(t.contains("Unable to clone."));
    assert!(t.contains("Check the URL and how you signed in, then try again."));
    assert!(t.contains("not found"));
}

#[test]
fn failed_install_offers_try_again() {
    let mut r = Rig::demo();
    r.app = Rig::new(Arc::new(DemoBackend::instant().failing_at(5)), (120, 40), false).app;
    r.to(Step::Review);
    r.erase_and_install("/dev/nvme0n1");
    r.settle();
    assert!(matches!(r.app.install_outcome(), Outcome::Failed { .. }));
    r.press(KeyCode::Enter);
    assert_eq!(r.app.step(), Step::Review);
}

#[test]
fn erase_needs_the_exact_device_path() {
    let mut r = Rig::demo();
    r.to(Step::Review);
    r.press(KeyCode::Enter);
    r.typ("/dev/nvme0n");
    r.press(KeyCode::Enter);
    assert_eq!(r.app.step(), Step::Review);
    assert!(r.app.modal_open());
    r.typ("1");
    r.press(KeyCode::Enter);
    assert_eq!(r.app.step(), Step::Install);
}

#[test]
fn ctrl_c_quits_through_a_modal_and_is_ignored_while_installing() {
    let mut r = Rig::demo();
    r.ctrl_c();
    assert!(r.app.modal_open());
    r.press(KeyCode::Left);
    assert_eq!(r.press(KeyCode::Enter), Some(Exit::Quit));

    let (be, release) = scripted(false);
    let mut r = Rig::new(be, (120, 40), false);
    r.to(Step::Review);
    r.erase_and_install("/dev/nvme0n1");
    r.wait_for("Step 3 of 12");
    r.ctrl_c();
    assert!(!r.app.modal_open());
    assert!(r.text((120, 40)).contains("cannot be stopped"));
    release.send(()).unwrap();
    r.settle();
}

#[test]
fn account_validation_blocks_and_explains() {
    let mut r = Rig::demo();
    r.to(Step::Account);
    r.typ("Kyan");
    r.press(KeyCode::Enter);
    assert_eq!(r.app.step(), Step::Account);
    let t = r.text((120, 40));
    assert!(t.contains("Use at least 8 characters."), "{t}");
    r.press(KeyCode::Char('x'));
    for _ in 0..2 {
        r.press(KeyCode::Tab);
    }
    assert!(r.text((120, 40)).contains("Use at least 8 characters."));
}

#[test]
fn computer_name_rejects_uppercase() {
    let mut r = Rig::demo();
    r.to(Step::Account);
    r.typ("Kyan");
    r.press(KeyCode::Tab);
    r.press(KeyCode::Tab);
    r.press(KeyCode::Char('X'));
    r.press(KeyCode::Tab);
    assert!(r.text((120, 40)).contains("Use lowercase letters, digits and hyphens."));
}

#[test]
fn login_follows_the_name_until_edited() {
    let mut r = Rig::demo();
    r.to(Step::Account);
    r.typ("Álvaro Núñez");
    assert!(r.text((120, 40)).contains("lvaro-fedora"));
    r.press(KeyCode::Tab);
    r.typ("x");
    r.press(KeyCode::Up);
    r.typ("a");
    let t = r.text((120, 40));
    assert!(t.contains("lvarox-fedora"), "{t}");
}

#[test]
fn encryption_needs_a_matching_passphrase() {
    let mut r = Rig::demo();
    r.to(Step::Disk);
    r.press(KeyCode::Char(' '));
    r.press(KeyCode::Enter);
    assert_eq!(r.app.step(), Step::Disk);
    let t = r.text((120, 40));
    assert!(t.contains("Use at least 8 characters."));
    r.typ("longenough");
    r.press(KeyCode::Tab);
    r.typ("different1");
    r.press(KeyCode::Enter);
    assert!(r.text((120, 40)).contains("The passphrases do not match."));
    r.press(KeyCode::BackTab);
    r.press(KeyCode::BackTab);
    r.press(KeyCode::BackTab);
    for _ in 0..3 {
        r.press(KeyCode::Tab);
    }
    for _ in 0..10 {
        r.press(KeyCode::Backspace);
    }
    r.typ("longenough");
    r.press(KeyCode::Enter);
    assert_eq!(r.app.step(), Step::System);
    assert!(r.app.draft().disk.unwrap().encrypt.is_some());
}

#[test]
fn live_medium_is_not_selectable() {
    let mut r = Rig::demo();
    r.to(Step::Disk);
    for _ in 0..5 {
        r.press(KeyCode::Down);
    }
    r.press(KeyCode::Enter);
    assert_eq!(r.app.draft().disk.map(|d| d.device), Some("/dev/sda".into()));
}

#[test]
fn desktop_choice_follows_the_preset() {
    let mut r = Rig::demo();
    r.to(Step::Preset);
    r.press(KeyCode::Down);
    r.press(KeyCode::Right);
    r.press(KeyCode::Enter);
    r.account();
    let c = r.app.draft().complete().unwrap();
    assert_eq!(
        c.system,
        SystemChoice::Fresh { distro: "fedora", preset: Preset::Desktop, desktop: Some(Desktop::Kde) }
    );
}

#[test]
fn review_change_links_jump_back() {
    let mut r = Rig::demo();
    r.to(Step::Review);
    r.press(KeyCode::Char('2'));
    assert_eq!(r.app.step(), Step::Disk);
}

#[test]
fn clicking_a_done_step_in_the_trail_goes_back() {
    let mut r = Rig::demo();
    r.to(Step::Account);
    let t = r.text((120, 40));
    let x = t.lines().next().unwrap().find("Disk").unwrap();
    let col = t.lines().next().unwrap()[..x].chars().count() as u16;
    r.click(col, 0);
    assert_eq!(r.app.step(), Step::Disk);
}

#[test]
fn clicking_a_selected_row_continues() {
    let mut r = Rig::demo();
    r.to(Step::System);
    r.text((120, 40));
    let t = r.text((120, 40));
    let (y, line) = t.lines().enumerate().find(|(_, l)| l.contains("Arch Linux")).unwrap();
    let x = line.chars().take_while(|c| *c != 'A').count() as u16;
    r.click(x, y as u16);
    assert_eq!(r.app.step(), Step::System);
    r.click(x, y as u16);
    assert_eq!(r.app.step(), Step::Preset);
}

#[test]
fn typing_filters_the_language_list() {
    let mut r = Rig::demo();
    r.press(KeyCode::Enter);
    r.typ("deu");
    let t = r.text((120, 40));
    assert!(t.contains("Deutsch (Deutschland)"));
    assert!(!t.contains("Français"));
    r.press(KeyCode::Esc);
    assert_eq!(r.app.step(), Step::Language);
    assert!(r.text((120, 40)).contains("Français"));
}

#[test]
fn keyboard_filter_loads_the_layout_only_when_leaving_the_column() {
    let (be, _release) = scripted(false);
    let mut r = Rig::new(be.clone(), (120, 40), false);
    r.to(Step::Language);
    r.press(KeyCode::Tab);
    let before = be.loaded.lock().unwrap().len();
    r.typ("ES");
    assert_eq!(be.loaded.lock().unwrap().len(), before, "a layout loaded mid-filter");
    assert!(r.text((120, 40)).contains("Spanish"));
    r.press(KeyCode::Tab);
    assert_eq!(be.loaded.lock().unwrap().last().map(String::as_str), Some("es"));
    assert_eq!(r.app.draft().locale.unwrap().keymap, "es");
}

#[test]
fn small_window_asks_for_more_room() {
    let mut r = Rig::demo();
    let t = r.text((70, 20));
    assert!(t.contains("Make the window at least 80 by 24."));
}

#[test]
fn spec_pane_shows_choices_and_marks_new_lines() {
    let mut r = Rig::demo();
    r.press(KeyCode::Enter);
    r.press(KeyCode::Down);
    r.now += Duration::from_millis(10);
    r.press(KeyCode::Down);
    let fresh = r.text_at((120, 40), r.now + Duration::from_millis(100));
    assert!(fresh.contains("spec.star"));
    assert!(fresh.contains("locale ="));
    assert!(fresh.contains("+ "), "{fresh}");
    let later = r.text_at((120, 40), r.now + Duration::from_secs(2));
    assert!(!later.lines().any(|l| l.contains("│ +")), "{later}");
}

#[test]
fn spec_overlay_toggles_below_110_columns() {
    let mut r = Rig::new(Arc::new(DemoBackend::instant()), (100, 30), false);
    r.press(KeyCode::Enter);
    r.press(KeyCode::Enter);
    r.press(KeyCode::Enter);
    assert_eq!(r.app.step(), Step::System);
    assert!(!r.text((100, 30)).contains("spec.star"));
    r.press(KeyCode::Char('s'));
    assert!(r.text((100, 30)).contains("spec.star"));
    r.press(KeyCode::Char('s'));
    assert!(!r.text((100, 30)).contains("spec.star"));
}

#[test]
fn screen_change_slides_then_goes_quiet() {
    let mut r = Rig::new(Arc::new(DemoBackend::instant()), (120, 40), true);
    assert_eq!(r.app.next_wake(r.now), None);
    r.press(KeyCode::Enter);
    assert_eq!(r.app.next_wake(r.now), Some(Duration::from_millis(16)));
    let early = r.text_at((120, 40), r.now + Duration::from_millis(10));
    let done = r.text_at((120, 40), r.now + Duration::from_millis(400));
    assert_ne!(early, done);
    assert_eq!(r.app.next_wake(r.now + Duration::from_millis(900)), None);
    let _ = r.t0;
}

// ---- goldens ----

#[test]
fn golden_welcome() {
    shots(&mut Rig::demo(), "welcome");
}

#[test]
fn golden_language() {
    let mut r = Rig::demo();
    r.to(Step::Language);
    shots(&mut r, "language");
}

#[test]
fn golden_disk() {
    let mut r = Rig::demo();
    r.to(Step::Disk);
    shots(&mut r, "disk");
}

#[test]
fn golden_disk_encrypted() {
    let mut r = Rig::demo();
    r.to(Step::Disk);
    r.press(KeyCode::Char(' '));
    r.press(KeyCode::Enter);
    shots(&mut r, "disk-encrypted");
}

#[test]
fn golden_system() {
    let mut r = Rig::demo();
    r.to(Step::System);
    r.press(KeyCode::Down);
    r.press(KeyCode::Down);
    shots(&mut r, "system");
}

#[test]
fn golden_preset() {
    let mut r = Rig::demo();
    r.to(Step::Preset);
    r.press(KeyCode::Down);
    shots(&mut r, "preset");
}

#[test]
fn golden_account() {
    let mut r = Rig::demo();
    r.to(Step::Account);
    r.typ("Kyan de Sutter");
    r.press(KeyCode::Tab);
    r.press(KeyCode::Tab);
    r.press(KeyCode::Tab);
    r.typ("short");
    r.press(KeyCode::Enter);
    shots(&mut r, "account");
}

#[test]
fn golden_review() {
    let mut r = Rig::demo();
    r.to(Step::Review);
    shots(&mut r, "review");
    r.press(KeyCode::Char(' '));
    r.press(KeyCode::Char(' '));
    r.press(KeyCode::Tab);
    r.press(KeyCode::Char(' '));
    shots(&mut r, "review-secure-boot");
}

#[test]
fn golden_review_modal() {
    let mut r = Rig::demo();
    r.to(Step::Review);
    r.press(KeyCode::Enter);
    r.typ("/dev/nvme");
    shots(&mut r, "review-modal");
}

#[test]
fn golden_quit_modal() {
    let mut r = Rig::demo();
    r.to(Step::Disk);
    r.ctrl_c();
    shots(&mut r, "quit-modal");
}

#[test]
fn golden_install_running() {
    let (be, release) = scripted(false);
    let mut r = Rig::new(be, (120, 40), false);
    r.to(Step::Review);
    r.erase_and_install("/dev/nvme0n1");
    r.wait_for("Step 3 of 12");
    let at = r.now + Duration::from_secs(83);
    for (tag, size) in SIZES {
        let t = r.text_at(size, at);
        golden(&format!("install-running-{tag}"), &t);
    }
    release.send(()).unwrap();
    r.settle();
}

#[test]
fn golden_install_done() {
    let mut r = Rig::demo();
    r.to(Step::Review);
    r.erase_and_install("/dev/nvme0n1");
    r.settle();
    shots(&mut r, "install-done");
}

#[test]
fn golden_install_failed() {
    let (be, _release) = scripted(true);
    let mut r = Rig::new(be, (120, 40), false);
    r.to(Step::Review);
    r.erase_and_install("/dev/nvme0n1");
    r.wait_for("The install stopped");
    shots(&mut r, "install-failed");
}

#[test]
fn golden_source() {
    let mut r = Rig::demo();
    r.press(KeyCode::Right);
    r.press(KeyCode::Enter);
    r.press(KeyCode::Enter);
    shots(&mut r, "source");
    r.press(KeyCode::Tab);
    r.press(KeyCode::Right);
    r.typ("");
    shots(&mut r, "source-ssh");
}

#[test]
fn golden_source_error() {
    let mut r = Rig::demo();
    r.press(KeyCode::Right);
    r.press(KeyCode::Enter);
    r.press(KeyCode::Enter);
    r.typ("git@example.com:fail/spec.git");
    r.press(KeyCode::Enter);
    r.settle();
    shots(&mut r, "source-error");
}

#[test]
fn golden_host() {
    let mut r = Rig::demo();
    r.press(KeyCode::Right);
    r.press(KeyCode::Enter);
    r.press(KeyCode::Enter);
    r.typ("git@github.com:me/spec.git");
    r.press(KeyCode::Enter);
    r.settle();
    shots(&mut r, "host");
}

#[test]
fn golden_account_restore() {
    let mut r = Rig::demo();
    r.press(KeyCode::Right);
    r.press(KeyCode::Enter);
    r.press(KeyCode::Enter);
    r.typ("git@github.com:me/spec.git");
    r.press(KeyCode::Enter);
    r.settle();
    r.press(KeyCode::Enter);
    r.press(KeyCode::Enter);
    shots(&mut r, "account-restore");
}

#[test]
fn golden_help_and_small() {
    let mut r = Rig::demo();
    r.to(Step::System);
    r.press(KeyCode::Char('?'));
    golden("help-120x40", &r.text((120, 40)));
    golden("too-small", &r.text((70, 20)));
}
