//! Everything the front end asks of the machine, behind one trait so the front end runs against a
//! demo backend on any computer and against the live one on the installer ISO.

use std::path::Path;
use std::sync::mpsc::Sender;

use anyhow::Result;

use crate::keys::{self, Key};
use crate::model::{Choices, DiskInfo, Draft, Firmware, GitAuth, HostSummary, Progress};

pub trait Backend: Send + Sync {
    fn disks(&self) -> Result<Vec<DiskInfo>>;
    fn firmware(&self) -> Firmware;
    /// `(locale, its name in that language)`, e.g. `("de_DE.UTF-8", "Deutsch (Deutschland)")`.
    fn locales(&self) -> Vec<(String, String)>;
    /// `(XKB layout, its description)`, e.g. `("es", "Spanish")`.
    fn keymaps(&self) -> Vec<(String, String)>;
    fn timezones(&self) -> Vec<String>;
    /// Load a console keymap now, so typing on the next screens matches it.
    fn set_keymap(&self, keymap: &str) -> Result<()>;
    /// Clone `url` into `into` and list its hosts.
    fn clone_spec(&self, url: &str, auth: &GitAuth, into: &Path) -> Result<Vec<HostSummary>>;
    /// Read a key for the spec's secrets. Blocks while it reaches another machine.
    fn fetch_key(&self, source: &keys::Source) -> Result<Key> {
        keys::read_local(source)
    }
    /// The secrets in the spec at `spec_dir` and whether `key` opens each; see [`keys::coverage`].
    fn secrets(&self, spec_dir: &Path, key: Option<&Key>) -> Result<Option<Vec<(String, bool)>>> {
        keys::coverage(spec_dir, key)
    }
    /// The `spec.star` the installer would write for `draft`, as far as it is filled in.
    fn preview(&self, draft: &Draft) -> String;
    /// Run the install, reporting on `tx` until `Progress::Done` or `Progress::Failed`. Blocks.
    fn install(&self, choices: &Choices, tx: Sender<Progress>);
    /// The first screen is on the terminal.
    fn shown(&self) {}
}
