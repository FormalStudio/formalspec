use std::fs;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};

use age::armor::ArmoredReader;
use anyhow::{Context, Result, bail};
use fspec_core::paths::Paths;
use fspec_core::resource::{Desired, Resource, Scope};
use fspec_core::state::State;

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
        identities.extend(parse_identities(&bytes, &file.display().to_string())?);
    }
    if identities.is_empty() {
        let tried: Vec<_> = files.iter().map(|f| f.display().to_string()).collect();
        bail!("no age identity found (tried {})", tried.join(", "));
    }
    Ok(identities)
}

/// The identities in one identity file's bytes: an unencrypted SSH private key or an age identity
/// file. `name` labels errors.
pub fn parse_identities(bytes: &[u8], name: &str) -> Result<Vec<Box<dyn age::Identity>>> {
    if bytes.starts_with(b"-----BEGIN") {
        match age::ssh::Identity::from_buffer(bytes, Some(name.to_string()))
            .with_context(|| format!("reading the SSH key {name}"))?
        {
            id @ age::ssh::Identity::Unencrypted(_) => Ok(vec![Box::new(id)]),
            _ => bail!("{name} is a passphrase-protected or unsupported SSH key"),
        }
    } else {
        let found = age::IdentityFile::from_buffer(BufReader::new(bytes))
            .with_context(|| format!("reading {name}"))?
            .into_identities()
            .with_context(|| format!("loading {name}"))?;
        Ok(found.into_iter().map(|i| i as Box<dyn age::Identity>).collect())
    }
}

/// Leave out the secrets of `scope` that no identity here decrypts, so the rest of an apply still
/// runs on a machine that has no key yet. Returns what is left and, for each secret left out, its
/// path and why. A secret `state` already holds stays in, since leaving it out would remove it.
pub fn without_undecryptable(
    paths: &Paths,
    scope: Scope,
    spec_dir: &Path,
    state: &State,
    desired: Vec<Desired>,
) -> (Vec<Desired>, Vec<(String, String)>) {
    let mut skipped = Vec::new();
    let kept = desired
        .into_iter()
        .filter(|d| {
            let Resource::Secret(s) = &d.resource else { return true };
            if d.resource.scope() != scope || state.entries.contains_key(&d.resource.key()) {
                return true;
            }
            let opened =
                identity_paths(paths, scope, &s.identities).and_then(|ids| decrypt_file(&spec_dir.join(&s.src), &ids));
            match opened {
                Ok(_) => true,
                Err(e) => {
                    skipped.push((s.path.clone(), format!("{e:#}")));
                    false
                }
            }
        })
        .collect();
    (kept, skipped)
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
