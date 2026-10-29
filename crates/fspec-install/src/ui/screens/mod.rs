pub mod account;
pub mod disk;
pub mod host;
pub mod install;
pub mod keys;
pub mod language;
pub mod modal;
pub mod preset;
pub mod review;
pub mod source;
pub mod system;
pub mod welcome;

use std::path::PathBuf;
use std::sync::Arc;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::backend::Backend;
use crate::catalog;
use crate::model::{Desktop, Distro, Firmware, GitAuth, HostSummary, Preset};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Step {
    Welcome,
    Language,
    Source,
    Host,
    Keys,
    Disk,
    System,
    Preset,
    Account,
    Review,
    Install,
}

impl Step {
    pub fn title(self) -> &'static str {
        match self {
            Step::Welcome => "Welcome",
            Step::Language => "Language",
            Step::Source => "Source",
            Step::Host => "Host",
            Step::Keys => "Keys",
            Step::Disk => "Disk",
            Step::System => "System",
            Step::Preset => "Preset",
            Step::Account => "Account",
            Step::Review => "Review",
            Step::Install => "Install",
        }
    }
}

pub fn flow(restore: bool) -> &'static [Step] {
    use Step::*;
    if restore {
        &[Language, Source, Host, Keys, Disk, Account, Review, Install]
    } else {
        &[Language, Disk, System, Preset, Account, Review, Install]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    Restart,
    Shell,
    Quit,
}

pub enum Flow {
    Stay,
    Next,
    Back,
    Jump(Step),
    Modal(modal::Modal),
    Clone,
    FetchKey,
    Install,
    Exit(Exit),
}

pub type Hints = Vec<(&'static str, &'static str)>;

pub fn hints(items: &[(&'static str, &'static str)]) -> Hints {
    items.to_vec()
}

/// Choices the screens share.
pub struct Shared {
    pub be: Arc<dyn Backend>,
    pub restore: bool,
    pub distro: Option<&'static Distro>,
    pub preset: Preset,
    pub desktop: Option<Desktop>,
    pub host: Option<HostSummary>,
    pub url: String,
    pub auth: GitAuth,
    pub checkout: PathBuf,
    pub firmware: Firmware,
}

impl Shared {
    pub fn subvolume(&self) -> String {
        format!("@{}", self.distro.map_or("system", |d| d.id))
    }

    pub fn set_host(&mut self, host: Option<HostSummary>) {
        self.distro = host.as_ref().and_then(|h| catalog::distro(&h.distro));
        self.host = host;
    }
}

pub fn is_enter(k: &KeyEvent) -> bool {
    k.code == KeyCode::Enter
}

pub fn plain(k: &KeyEvent) -> bool {
    !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
}

/// Next focus index with Tab, previous with Shift+Tab, over `n` stops.
pub fn tab_focus(k: &KeyEvent, cur: usize, n: usize) -> Option<usize> {
    match k.code {
        KeyCode::Tab => Some((cur + 1) % n),
        KeyCode::BackTab => Some((cur + n - 1) % n),
        _ => None,
    }
}
