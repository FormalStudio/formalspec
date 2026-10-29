//! `fspec secret edit`, `show`, `list` and `rekey`: agenix's `-e`, `-d` and `-r` against `secrets.star`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use fspec_core::{Paths, Scope};
use fspec_files::secret;

type Recipients = BTreeMap<String, Vec<String>>;

fn recipients(spec_dir: &Path) -> Result<Recipients> {
    fspec_eval::evaluate_recipients(&spec_dir.join("secrets.star"))
}

fn identities(paths: &Paths, given: &[PathBuf]) -> Result<Vec<PathBuf>> {
    if given.is_empty() { secret::identity_paths(paths, Scope::User, &[]) } else { Ok(given.to_vec()) }
}

/// The secrets.star entry `file` names: the entry itself, or its file name with or without `.age`.
fn entry<'r>(all: &'r Recipients, file: &str) -> Result<(&'r String, &'r Vec<String>)> {
    let named = |k: &str| {
        let base = k.rsplit('/').next().unwrap_or(k);
        k == file || base == file || base.strip_suffix(".age") == Some(file)
    };
    let mut hits = all.iter().filter(|(k, _)| named(k));
    match (hits.next(), hits.next()) {
        (Some(hit), None) => Ok(hit),
        (None, _) => bail!("{file} is not in secrets.star"),
        (Some(_), Some(_)) => bail!("{file} names more than one entry in secrets.star"),
    }
}

pub fn edit(paths: &Paths, spec_dir: &Path, file: &str, given: &[PathBuf]) -> Result<()> {
    let all = recipients(spec_dir)?;
    let (name, keys) = entry(&all, file)?;
    let path = spec_dir.join(name);
    let plain = if path.exists() { secret::decrypt_file(&path, &identities(paths, given)?)? } else { Vec::new() };
    // A private directory, so the plaintext is never readable by anyone else while it is open.
    let dir = tempfile::Builder::new().prefix("fspec-secret").tempdir()?;
    let scratch = dir.path().join(name.rsplit('/').next().unwrap_or(name).trim_end_matches(".age"));
    fs::write(&scratch, &plain)?;
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("sh")
        .arg(&scratch)
        .status()
        .with_context(|| format!("running {editor}"))?;
    if !status.success() {
        bail!("{editor} exited {status}; {name} is unchanged");
    }
    let edited = fs::read(&scratch)?;
    if edited == plain && path.exists() {
        println!("{name} unchanged");
        return Ok(());
    }
    fs::write(&path, secret::encrypt(&edited, keys)?).with_context(|| format!("writing {}", path.display()))?;
    println!("wrote {name}");
    Ok(())
}

pub fn show(paths: &Paths, spec_dir: &Path, file: &str, given: &[PathBuf]) -> Result<()> {
    let all = recipients(spec_dir)?;
    let (name, _) = entry(&all, file)?;
    let plain = secret::decrypt_file(&spec_dir.join(name), &identities(paths, given)?)?;
    std::io::Write::write_all(&mut std::io::stdout().lock(), &plain)?;
    Ok(())
}

pub fn list(spec_dir: &Path) -> Result<()> {
    for (name, keys) in &recipients(spec_dir)? {
        let missing = if spec_dir.join(name).exists() { "" } else { " (missing)" };
        let short: Vec<_> = keys
            .iter()
            .map(|k| k.split_whitespace().take(2).last().unwrap_or(k))
            .map(|k| &k[..k.len().min(16)])
            .collect();
        println!("{name}{missing}: {}", short.join(", "));
    }
    Ok(())
}

pub fn rekey(paths: &Paths, spec_dir: &Path, given: &[PathBuf]) -> Result<()> {
    let ids = secret::load_identities(&identities(paths, given)?)?;
    for (name, keys) in &recipients(spec_dir)? {
        let path = spec_dir.join(name);
        if !path.exists() {
            println!("skipped {name}: it does not exist");
            continue;
        }
        let ciphertext = fs::read(&path)?;
        let plain = secret::decrypt(&ciphertext, &ids).with_context(|| name.clone())?;
        fs::write(&path, secret::encrypt(&plain, keys)?).with_context(|| format!("writing {}", path.display()))?;
        println!("rekeyed {name}");
    }
    Ok(())
}
