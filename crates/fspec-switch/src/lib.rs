//! `fspec switch` and `fspec forget`. Each operation is a pure function that returns a [`Plan`]
//! of commands; the CLI hands the plan to a [`Runner`], which under `--root` only logs it.

mod adopt;
mod beside;
mod bootc;
mod classic;
mod disk;
mod forget;
mod machine;
mod recipe;
#[cfg(test)]
mod tests;
mod tumbleweed;
mod windows;

use anyhow::{Result, bail};
use fspec_core::journal::{Journal, Undo};
use fspec_core::runner::{Cmd, Runner};

pub use adopt::adopt_boot;
pub use beside::{Beside, beside};
pub use bootc::{ResetVar, has_staged, is_bootc, reset_var, to_image};
pub use classic::{Target, switch_distro, title};
pub use disk::*;
pub use forget::forget;
pub use machine::{
    Btrfs, Facts, Firmware, GIB, LOADER_ESP, Layout, Machine, Part, XBOOTLDR_TYPE, assumed, boot_owner, classify,
    detect, detect_device,
};
pub use recipe::{Recipe, caveats, misfit, recipe};
pub use tumbleweed::{
    ESP_SYNC as TUMBLEWEED_ESP_SYNC, ESP_SYNC_PATH as TUMBLEWEED_ESP_SYNC_PATH, TUMBLEWEED_IMAGE, Tumbleweed,
    tumbleweed,
};
pub use windows::{
    BesideWindows, GB, Gap, MIN_LINUX, Shrink, Space, WINDOWS_HEADROOM, WindowsDisk, XBOOTLDR_SIZE, ntfs_info,
    read_windows, shrink_min, space,
};

/// The distros `fspec switch --distro` can bootstrap.
pub const DISTROS: [&str; 6] = ["fedora", "arch", "debian", "ubuntu", "void", "aerynos"];

/// The runner could not start the program because it is not installed.
pub(crate) fn not_installed(e: &anyhow::Error) -> bool {
    e.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
}

pub(crate) fn subvol(distro: &str) -> String {
    format!("@{distro}")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    Run(Cmd),
    /// A read-only probe whose exit status must match `ok`, else the plan stops with `msg`.
    Expect {
        probe: Cmd,
        ok: bool,
        msg: String,
    },
    /// Journaled when reached, so a failure from here on, or a run killed from here on, runs it.
    Undo(Cmd),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub steps: Vec<Step>,
    /// Unmounts, run after the steps whether or not they succeeded.
    pub cleanup: Vec<Cmd>,
}

impl Plan {
    pub(crate) fn run(&mut self, cmd: Cmd) {
        self.steps.push(Step::Run(cmd));
    }

    pub(crate) fn undo(&mut self, cmd: Cmd) {
        self.steps.push(Step::Undo(cmd));
    }

    pub(crate) fn expect(&mut self, probe: Cmd, ok: bool, msg: impl Into<String>) {
        self.steps.push(Step::Expect { probe, ok, msg: msg.into() });
    }

    /// The plan with `dir` mounted on itself as a private mount around it. What the plan mounts
    /// under `dir` then stays out of the namespaces of services that start meanwhile, where a copy
    /// would keep the filesystem busy after the plan unmounts it.
    pub(crate) fn private(mut self, dir: &str) -> Plan {
        let mut steps = vec![
            Step::Run(Cmd::new("mkdir").arg("-p").arg(dir)),
            Step::Run(Cmd::new("mount").arg("--bind").arg(dir).arg(dir)),
            Step::Run(Cmd::new("mount").arg("--make-private").arg(dir)),
        ];
        steps.append(&mut self.steps);
        self.steps = steps;
        self.cleanup.push(Cmd::new("umount").arg(dir));
        self
    }

    /// Every command in execution order, probes and cleanup included, and undo steps as `undo <cmd>`.
    pub fn lines(&self) -> Vec<String> {
        let steps = self.steps.iter().map(|s| match s {
            Step::Run(c) => c.to_string(),
            Step::Expect { probe, .. } => probe.to_string(),
            Step::Undo(c) => format!("undo {c}"),
        });
        steps.chain(self.cleanup.iter().map(ToString::to_string)).collect()
    }

    /// Hand every command to `runner` and ignore the answers. Under `--root` the runner only logs,
    /// so probes cannot steer the plan.
    pub fn log(&self, runner: &dyn Runner) -> Result<()> {
        let all = self.steps.iter().filter_map(|s| match s {
            Step::Run(c) => Some(c),
            Step::Expect { probe, .. } => Some(probe),
            Step::Undo(_) => None,
        });
        all.chain(&self.cleanup).try_for_each(|c| runner.run(c).map(drop))
    }

    pub fn execute(&self, runner: &dyn Runner) -> Result<()> {
        if self.steps.iter().any(|s| matches!(s, Step::Undo(_))) {
            bail!("this plan has undo steps, which need a journal");
        }
        self.run_steps(runner, None)
    }

    /// Run the plan, and when a step fails, run the undo steps reached so far, newest first, after
    /// the cleanup has unmounted what they would undo. Only for undo a killed run can do without,
    /// such as deleting a half-built root; the rest needs [`Plan::execute_journaled`].
    pub fn execute_undoing(&self, runner: &dyn Runner) -> Result<()> {
        self.run_steps(runner, None)
    }

    /// Run the plan, recording its undo steps in `journal` as they are reached. The caller rolls
    /// the journal back on failure, after the cleanup has unmounted what the plan mounted.
    pub fn execute_journaled(&self, runner: &dyn Runner, journal: &mut Journal) -> Result<()> {
        self.run_steps(runner, Some(journal))
    }

    fn run_steps(&self, runner: &dyn Runner, mut journal: Option<&mut Journal>) -> Result<()> {
        let mut reached = Vec::new();
        let result = self.steps.iter().try_for_each(|s| match s {
            Step::Run(c) => runner.check(c).map(drop),
            Step::Undo(c) => match journal.as_deref_mut() {
                Some(j) => j.record(Undo::Run { cmd: c.clone() }),
                None => {
                    reached.push(c);
                    Ok(())
                }
            },
            Step::Expect { probe, ok, msg } => {
                if (runner.run(probe)?.status == 0) == *ok {
                    Ok(())
                } else {
                    bail!("{msg}")
                }
            }
        });
        // A failed unmount must not hide the error that stopped the steps.
        for c in &self.cleanup {
            let _ = runner.run(c);
        }
        if let Err(e) = result {
            for c in reached.iter().rev() {
                if let Err(u) = runner.check(c) {
                    return Err(e.context(format!("undoing `{c}` failed too: {u:#}")));
                }
            }
            return Err(e);
        }
        Ok(())
    }
}
