//! Keys for the spec's secrets: brought from another disk or machine, or made here, checked
//! against `secrets.star`, and installed where `secret(...)` looks by default.

use std::path::{Path, PathBuf};

use age::secrecy::ExposeSecret;
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use fspec_core::runner::{Cmd, Runner};

use crate::model::Secret;

/// ssh runs the installer itself as its SSH_ASKPASS program; it prints this variable and exits.
pub const ASKPASS_ENV: &str = "FSPEC_INSTALL_ASKPASS";

/// Where a key comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// Typed or pasted: an age identity or an SSH private key.
    Paste(Secret),
    /// A file on a mounted disk, such as a USB stick.
    File(PathBuf),
    /// `user@host:path` on another machine, read with `cat` over ssh. Without a password, ssh
    /// tries the live session's own keys only.
    Ssh { from: String, password: Option<Secret> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Age,
    Ed25519,
    Rsa,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Key {
    pub kind: Kind,
    /// The identity file's text.
    pub private: Secret,
    /// `age1...` for each age identity in it, or the SSH key's `ssh-ed25519 AAAA...` line.
    pub public: Vec<String>,
    /// Made by the installer, so nothing in the spec is encrypted to it yet.
    pub generated: bool,
}

impl Key {
    /// An age identity file (one or more `AGE-SECRET-KEY-1...` lines) or an unencrypted ed25519 or
    /// RSA SSH private key, the identities `secret(...)` decrypts with.
    pub fn parse(bytes: &[u8]) -> Result<Key> {
        let text = std::str::from_utf8(bytes).context("the key is not text")?.trim();
        if text.starts_with("-----BEGIN") {
            let private = format!("{text}\n");
            // Refuses a passphrase-protected key, which fspec cannot open at boot.
            fspec_files::secret::parse_identities(private.as_bytes(), "the SSH key")?;
            let (kind, public) = match openssh_public(text) {
                Some((t, wire)) if t == "ssh-ed25519" => {
                    (Kind::Ed25519, vec![format!("{t} {}", STANDARD.encode(wire))])
                }
                Some((t, wire)) if t == "ssh-rsa" => (Kind::Rsa, vec![format!("{t} {}", STANDARD.encode(wire))]),
                Some((t, _)) => bail!("fspec decrypts with ed25519 and RSA SSH keys only, not {t}"),
                None if text.contains("RSA PRIVATE KEY") => (Kind::Rsa, Vec::new()),
                None => bail!("fspec decrypts with ed25519 and RSA SSH keys only"),
            };
            return Ok(Key { kind, private: Secret(private), public, generated: false });
        }
        let mut public = Vec::new();
        for line in text.lines().map(str::trim).filter(|l| l.starts_with("AGE-SECRET-KEY-")) {
            let id: age::x25519::Identity =
                line.parse().map_err(|e: &str| anyhow!(e)).context("reading the age key")?;
            public.push(id.to_public().to_string());
        }
        if public.is_empty() {
            bail!("found no age identity (AGE-SECRET-KEY-1...) and no SSH private key");
        }
        Ok(Key { kind: Kind::Age, private: Secret(format!("{text}\n")), public, generated: false })
    }

    /// A new age identity, written as `age-keygen` writes one.
    pub fn generate() -> Key {
        let id = age::x25519::Identity::generate();
        let public = id.to_public().to_string();
        let private =
            format!("# created by fspec-install\n# public key: {public}\n{}\n", id.to_string().expose_secret());
        Key { kind: Kind::Age, private: Secret(private), public: vec![public], generated: true }
    }

    /// `age key age1qz5k…7x2f`, `SSH key (ed25519)`.
    pub fn label(&self) -> String {
        match self.kind {
            Kind::Age => {
                let first = self.public.first().map_or("", String::as_str);
                let short = if first.len() > 20 {
                    format!("{}…{}", &first[..10], &first[first.len() - 6..])
                } else {
                    first.into()
                };
                let more =
                    if self.public.len() > 1 { format!(" and {} more", self.public.len() - 1) } else { String::new() };
                format!("age key {short}{more}")
            }
            Kind::Ed25519 => "SSH key (ed25519)".into(),
            Kind::Rsa => "SSH key (RSA)".into(),
        }
    }

    /// The private key's place under the user's home, which is among `secret(...)`'s defaults.
    fn home_path(&self) -> &'static str {
        match self.kind {
            Kind::Age => ".config/fspec/age.key",
            Kind::Ed25519 => ".ssh/id_ed25519",
            Kind::Rsa => ".ssh/id_rsa",
        }
    }
}

/// The type and wire bytes of the public key in an OpenSSH private key file, which keeps them
/// unencrypted ahead of the private part.
fn openssh_public(text: &str) -> Option<(String, Vec<u8>)> {
    fn string<'b>(rest: &mut &'b [u8]) -> Option<&'b [u8]> {
        let len = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?) as usize;
        let s = rest.get(4..4 + len)?;
        *rest = &rest[4 + len..];
        Some(s)
    }
    let body: String = text.lines().filter(|l| !l.starts_with("-----")).collect();
    let raw = STANDARD.decode(body.trim()).ok()?;
    let mut rest = raw.strip_prefix(b"openssh-key-v1\0")?;
    for _ in 0..3 {
        string(&mut rest)?;
    }
    rest = rest.get(4..)?;
    let wire = string(&mut rest)?;
    let kind = String::from_utf8(string(&mut &wire[..])?.to_vec()).ok()?;
    Some((kind, wire.to_vec()))
}

/// Read a pasted key or a key file. A key on another machine needs [`fetch`].
pub fn read_local(source: &Source) -> Result<Key> {
    match source {
        Source::Paste(text) => Key::parse(text.0.as_bytes()),
        Source::File(path) => {
            let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
            Key::parse(&bytes).with_context(|| path.display().to_string())
        }
        Source::Ssh { from, .. } => bail!("{from} is on another machine"),
    }
}

/// Read a key from wherever `source` says. `askpass` is the program ssh asks for a password.
pub fn fetch(runner: &dyn Runner, source: &Source, askpass: &Path) -> Result<Key> {
    let Source::Ssh { from, password } = source else { return read_local(source) };
    let out = runner.check(&ssh_cmd(from, password.as_ref(), askpass)?)?;
    Key::parse(out.stdout.as_bytes()).with_context(|| from.clone())
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// `ssh user@host cat <path>`, with `~/` left for the remote shell to expand.
pub fn ssh_cmd(from: &str, password: Option<&Secret>, askpass: &Path) -> Result<Cmd> {
    let (host, path) = from
        .split_once(':')
        .filter(|(h, p)| !h.is_empty() && !h.starts_with('-') && !p.is_empty())
        .with_context(|| format!("{from} is not user@host:path"))?;
    let remote = match path.strip_prefix("~/") {
        Some(rest) => format!("cat ~/{}", quote(rest)),
        None => format!("cat {}", quote(path)),
    };
    let ssh = Cmd::new("ssh").args(["-o", "StrictHostKeyChecking=accept-new", "-o", "ConnectTimeout=15"]);
    let ssh = match password.filter(|p| !p.0.is_empty()) {
        Some(p) => ssh
            .args(["-o", "NumberOfPasswordPrompts=1"])
            .env("SSH_ASKPASS", askpass.display().to_string())
            .env("SSH_ASKPASS_REQUIRE", "force")
            .env(ASKPASS_ENV, p.0.clone()),
        None => ssh.args(["-o", "BatchMode=yes"]),
    };
    Ok(ssh.arg(host).arg(remote))
}

/// Each `secrets.star` entry whose `.age` file exists, and whether `key` decrypts it, in
/// `secrets.star` order. Without a key every entry reads `false`. `None` when the spec has no
/// `secrets.star`.
pub fn coverage(spec_dir: &Path, key: Option<&Key>) -> Result<Option<Vec<(String, bool)>>> {
    let star = spec_dir.join("secrets.star");
    if !star.exists() {
        return Ok(None);
    }
    let ids = match key {
        Some(k) => fspec_files::secret::parse_identities(k.private.0.as_bytes(), &k.label())?,
        None => Vec::new(),
    };
    let mut out = Vec::new();
    for name in fspec_eval::evaluate_recipients(&star)?.into_keys() {
        let Ok(ciphertext) = std::fs::read(spec_dir.join(&name)) else { continue };
        let opens = !ids.is_empty() && fspec_files::secret::decrypt(&ciphertext, &ids).is_ok();
        out.push((name, opens));
    }
    Ok(Some(out))
}

/// Writes stdin to `$1`, a path in the new root as seen from the live session, readable by root
/// only.
const WRITE_ROOT: &str = r#"umask 077 && mkdir -p "${1%/*}" && cat > "$1""#;

/// Run inside the new root: writes stdin to `$2` under user `$1`'s home with mode `$3`, the
/// directory private, and every part the user's.
const WRITE_HOME: &str = r#"set -e
cd "/home/$1"
dir=${2%/*}
mkdir -p "$dir"
chmod 700 "$dir"
(umask 077 && cat > "$2")
chmod "$3" "$2"
p=
for part in $(printf %s "$dir" | tr / ' '); do p=${p:+$p/}$part; chown "$1:" "$p"; done
chown "$1:" "$2""#;

/// The age keys as one identity file, or `None` without any.
fn age_file(keys: &[Key]) -> Option<String> {
    let age: Vec<&str> = keys.iter().filter(|k| k.kind == Kind::Age).map(|k| k.private.0.as_str()).collect();
    (!age.is_empty()).then(|| age.join(""))
}

/// Put root's copy of the age keys at `/root/.config/fspec/age.key` in the new root, mounted at
/// `root`, before its first apply. It is a default identity of the system scope, so system
/// secrets decrypt at that apply and at every boot after. SSH keys stay the user's: the system
/// scope's SSH identities are the host's own keys.
pub fn system_steps(root: &str, keys: &[Key]) -> Vec<Cmd> {
    let Some(text) = age_file(keys) else { return Vec::new() };
    vec![Cmd::new("sh").args(["-c", WRITE_ROOT, "sh"]).arg(format!("{root}/root/.config/fspec/age.key")).stdin(text)]
}

/// Give `user` the keys inside the new root, once the apply has made the account.
pub fn user_steps(root: &str, user: &str, keys: &[Key]) -> Vec<Cmd> {
    let mut files: Vec<(&str, String, &str)> = Vec::new();
    if let Some(text) = age_file(keys) {
        files.push((".config/fspec/age.key", text, "600"));
    }
    for k in keys.iter().filter(|k| k.kind != Kind::Age) {
        files.push((k.home_path(), k.private.0.clone(), "600"));
        if let Some(public) = k.public.first() {
            let pub_path = if k.kind == Kind::Ed25519 { ".ssh/id_ed25519.pub" } else { ".ssh/id_rsa.pub" };
            files.push((pub_path, format!("{public}\n"), "644"));
        }
    }
    files
        .into_iter()
        .map(|(path, text, mode)| {
            Cmd::new("chroot").arg(root).args(["sh", "-c", WRITE_HOME, "sh"]).arg(user).arg(path).arg(mode).stdin(text)
        })
        .collect()
}

/// Whether [`user_steps`] made `cmd`.
pub fn is_user_step(cmd: &Cmd) -> bool {
    cmd.program == "chroot" && cmd.args.iter().any(|a| a == WRITE_HOME)
}

/// What to do on a machine that already decrypts the secrets, so this one can too: the line for
/// `secrets.star` and the rekey.
pub fn rekey_steps(host: &str, key: &Key) -> Vec<String> {
    let name: String = host.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    let public = key.public.first().map_or("", String::as_str);
    vec![
        "This machine has a new age key. To let it decrypt your secrets,".into(),
        "on a machine that already can:".into(),
        "  1. Add this line to secrets.star:".into(),
        format!("     {name} = \"{public}\""),
        format!("  2. Add {name} to the list of each secret it needs."),
        "  3. Run fspec secret rekey, then commit and push.".into(),
        "  4. Here, pull the spec and run fspec apply.".into(),
    ]
}

/// The apply's warning for each secret it left out, as `fspec apply --skip-undecryptable-secrets`
/// prints it.
pub const SKIPPED: &str = "warning: skipped secret ";

/// The path of a secret the first apply left out, from its warning line.
pub fn skipped_secret(line: &str) -> Option<&str> {
    let rest = line.strip_prefix(SKIPPED)?;
    Some(rest.split_once(": ").map_or(rest, |(path, _)| path))
}

#[cfg(test)]
mod tests;
