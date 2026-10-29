use std::fs;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};

use age::armor::ArmoredReader;
use anyhow::{Context, Result, bail};
use fspec_core::paths::Paths;
use fspec_core::resource::Scope;

/// agenix's defaults: the host's SSH keys for a system secret, the user's for a user one, with
/// fspec's own age key as an extra.
pub fn default_identities(scope: Scope) -> Vec<String> {
    let ssh: &[&str] = match scope {
        Scope::System => &["/etc/ssh/ssh_host_ed25519_key", "/etc/ssh/ssh_host_rsa_key"],
        Scope::User => &["~/.ssh/id_ed25519", "~/.ssh/id_rsa"],
    };
    ssh.iter().copied().chain(["~/.config/fspec/age.key", "~/.config/age/keys.txt"]).map(str::to_string).collect()
}

/// Resolve identity paths as written in the spec; an empty list means the scope's defaults.
pub fn identity_paths(paths: &Paths, scope: Scope, declared: &[String]) -> Result<Vec<PathBuf>> {
    let declared = if declared.is_empty() { default_identities(scope) } else { declared.to_vec() };
    declared.iter().map(|p| paths.resolve(p)).collect()
}

/// Load every identity file that exists: SSH private keys (ed25519 or RSA, unencrypted, as agenix
/// needs at boot) and age identity files. Missing files are skipped, as agenix skips them.
pub fn load_identities(files: &[PathBuf]) -> Result<Vec<Box<dyn age::Identity>>> {
    let mut identities: Vec<Box<dyn age::Identity>> = Vec::new();
    for file in files.iter().filter(|f| f.exists()) {
        let bytes = fs::read(file).with_context(|| format!("reading the identity {}", file.display()))?;
        if bytes.starts_with(b"-----BEGIN") {
            let name = Some(file.display().to_string());
            match age::ssh::Identity::from_buffer(&bytes[..], name)
                .with_context(|| format!("reading the SSH key {}", file.display()))?
            {
                id @ age::ssh::Identity::Unencrypted(_) => identities.push(Box::new(id)),
                _ => bail!("{} is a passphrase-protected or unsupported SSH key", file.display()),
            }
        } else {
            let found = age::IdentityFile::from_buffer(BufReader::new(&bytes[..]))
                .with_context(|| format!("reading {}", file.display()))?
                .into_identities()
                .with_context(|| format!("loading {}", file.display()))?;
            identities.extend(found.into_iter().map(|i| i as Box<dyn age::Identity>));
        }
    }
    if identities.is_empty() {
        let tried: Vec<_> = files.iter().map(|f| f.display().to_string()).collect();
        bail!("no age identity found (tried {})", tried.join(", "));
    }
    Ok(identities)
}

/// Decrypt an age file's bytes (binary or armored).
pub fn decrypt(ciphertext: &[u8], identities: &[Box<dyn age::Identity>]) -> Result<Vec<u8>> {
    let decryptor = age::Decryptor::new(ArmoredReader::new(ciphertext)).context("not an age file")?;
    let mut reader = decryptor.decrypt(identities.iter().map(|i| i.as_ref())).context("decrypting")?;
    let mut plain = Vec::new();
    reader.read_to_end(&mut plain)?;
    Ok(plain)
}

/// Decrypt `src` with the identities in `files`.
pub fn decrypt_file(src: &Path, files: &[PathBuf]) -> Result<Vec<u8>> {
    let ciphertext = fs::read(src).with_context(|| format!("reading {}", src.display()))?;
    decrypt(&ciphertext, &load_identities(files)?).with_context(|| src.display().to_string())
}

/// Encrypt to `recipients`, each an age (`age1…`) or SSH (`ssh-ed25519 …`, `ssh-rsa …`) public
/// key, in the binary format agenix writes.
pub fn encrypt(plain: &[u8], recipients: &[String]) -> Result<Vec<u8>> {
    if recipients.is_empty() {
        bail!("no recipients");
    }
    let parsed = recipients
        .iter()
        .map(|r| -> Result<Box<dyn age::Recipient>> {
            if r.starts_with("ssh-") {
                Ok(Box::new(
                    r.parse::<age::ssh::Recipient>()
                        .map_err(|e| anyhow::anyhow!("{e:?}"))
                        .with_context(|| r.clone())?,
                ))
            } else {
                Ok(Box::new(
                    r.parse::<age::x25519::Recipient>().map_err(anyhow::Error::msg).with_context(|| r.clone())?,
                ))
            }
        })
        .collect::<Result<Vec<_>>>()?;
    let encryptor = age::Encryptor::with_recipients(parsed.iter().map(|r| r.as_ref()))?;
    let mut out = Vec::new();
    let mut writer = encryptor.wrap_output(&mut out)?;
    writer.write_all(plain)?;
    writer.finish()?;
    Ok(out)
}
