use anyhow::{Result, bail};
use fspec_core::Paths;
use fspec_core::runner::Cmd;

use crate::Plan;
use crate::classic::{Sources, grub_menu, s, sd_default};
use crate::machine::{Layout, Machine};

/// Make the running root own boot, so the root that owned it before can be forgotten: its entry
/// becomes systemd-boot's default, the pool's `bootfs`, or the shared GRUB menu's default with
/// GRUB reinstalled from this root.
pub fn adopt_boot(paths: &Paths, m: &Machine) -> Result<Plan> {
    let src = Sources::running(paths);
    let here = &m.running;
    let mut plan = Plan::default();
    match &m.layout {
        Layout::Default { .. } => {
            sd_default(&s(&paths.system(m.layout.loader_dir())), here).into_iter().for_each(|c| plan.run(c))
        }
        Layout::Zfs { pool, .. } => plan.run(Cmd::new("zpool").arg("set").arg(format!("bootfs={}", m.root)).arg(pool)),
        Layout::Bios { boot, .. } => {
            let install = if here == "fedora" { "grub2-install" } else { "grub-install" };
            plan.run(
                Cmd::new(install)
                    .arg("--target=i386-pc")
                    .arg(format!("--boot-directory={}", src.shared_boot))
                    .arg(&m.disk),
            );
            plan.run(grub_menu(&src.shared_boot, &boot.uuid, here));
        }
        Layout::Unsupported { reasons } => {
            bail!("this disk is not on a supported layout: {}; run `fspec disk plan` for the steps", reasons.join("; "))
        }
    }
    Ok(plan)
}
