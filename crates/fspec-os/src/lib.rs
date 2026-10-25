//! Service managers, accounts and macOS settings: the backends for `service`, `timer`, `user`,
//! `defaults` and `setting` resources. runit, OpenRC, dinit and s6 share `initsvc` and run timers as cron
//! entries through `crond`.

mod cron;
mod crond;
mod defaults;
mod dinit;
mod distro_unit;
mod initsvc;
mod launchd;
mod locale;
mod openrc;
mod runit;
mod s6;
mod setting;
mod systemd;
#[cfg(test)]
mod testutil;
mod users;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use fspec_core::Resource;
use fspec_core::facts::Init;

pub use defaults::DefaultsBackend;
pub use dinit::DinitBackend;
pub use launchd::LaunchdBackend;
pub use locale::LocaleBackend;
pub use openrc::OpenrcBackend;
pub use runit::RunitBackend;
pub use s6::S6Backend;
pub use setting::SettingBackend;
pub use systemd::SystemdBackend;
pub use users::UsersBackend;

/// Fails a service that sets `systemd = {...}` on an init other than systemd. Every non-systemd
/// service backend calls it from `check`, so the plan names the field instead of dropping it.
pub fn reject_init_extras(init: Init, r: &Resource) -> Result<()> {
    let Resource::Service(s) = r else { return Ok(()) };
    for (field, own, extras) in [("systemd", Init::Systemd, &s.systemd), ("launchd", Init::Launchd, &s.launchd)] {
        if init == own || extras.is_empty() {
            continue;
        }
        let keys: Vec<&str> = extras.keys().map(String::as_str).collect();
        bail!(
            "service:{} sets `{field}` ({}), which only {field} supports, and this host uses {init}",
            s.name,
            keys.join(", ")
        )
    }
    Ok(())
}

/// Service and timer names become file names and init labels.
fn check_name(kind: &str, name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && !name.starts_with(['.', '-'])
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | '@'));
    if !ok {
        bail!("{kind} name `{name}` may only hold letters, digits and `_ . - @`, and must not start with `.` or `-`");
    }
    Ok(())
}

fn check_line(what: &str, value: &str) -> Result<()> {
    if value.contains(['\n', '\r']) {
        bail!("{what} must be a single line");
    }
    Ok(())
}

fn write_file(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::write(path, content).with_context(|| format!("writing {}", path.display()))
}

fn read_or_empty(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok()
}

/// Fails unless `key` is a valid variable name for a shell script and `env`.
fn check_env_name(service: &str, key: &str) -> Result<()> {
    let ok = key.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !ok {
        bail!("env entry `{key}` of service {service} is not a valid variable name");
    }
    Ok(())
}

/// The argument vector for `exec`. Shell syntax (pipes, redirects, `$`, globs, `;`, `&`,
/// backticks) runs as `/bin/sh -c <exec>`; otherwise `exec` is split with shell rules, and a bare
/// command name goes through `/usr/bin/env` because these inits do not search `PATH`. Non-empty
/// `env` is set through a leading `/usr/bin/env`.
fn argv(exec: &str, env: &BTreeMap<String, String>) -> Result<Vec<String>> {
    check_line("exec", exec)?;
    let exec = exec.trim();
    let mut words = if exec.contains(['|', '&', ';', '<', '>', '(', ')', '$', '`', '*', '?', '[', '{', '~', '!', '#']) {
        vec!["/bin/sh".to_string(), "-c".into(), exec.into()]
    } else {
        shell_words::split(exec).with_context(|| format!("exec `{exec}`"))?
    };
    if words.is_empty() {
        bail!("exec is empty");
    }
    if !env.is_empty() {
        let pairs = env.iter().map(|(k, v)| format!("{k}={v}"));
        words.splice(0..0, ["/usr/bin/env".to_string()].into_iter().chain(pairs));
    } else if !words[0].starts_with('/') {
        words.insert(0, "/usr/bin/env".into());
    }
    Ok(words)
}
