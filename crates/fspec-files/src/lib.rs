//! The `file` and `secret` resources: managed copies, symlinks, seeds, three-way patches and
//! age-decrypted secrets.

pub mod adopt;
mod live;
pub mod patch;
mod render;
pub mod secret;
pub mod watch;

use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use fspec_core::Scope;
use fspec_core::apply::{Op, Records};
use fspec_core::facts::Init;
use fspec_core::journal::{Journal, Undo, remove_any};
use fspec_core::resource::{File, FileMode, Format, OnDrift, Resource, Secret};
use fspec_core::runner::Cmd;
use fspec_core::state::Entry;
use fspec_core::{Backend, Cx, Phase};
use serde_json::{Map, Value};

pub use live::{Live, drift_notes, read_live, sha256};

use patch::Record;

pub struct FileBackend;

impl Backend for FileBackend {
    fn name(&self) -> &'static str {
        "files"
    }

    fn handles(&self, _cx: &Cx, r: &Resource) -> bool {
        matches!(r, Resource::File(_) | Resource::Secret(_))
    }

    fn phase(&self) -> Phase {
        Phase::Files
    }

    fn check(&self, cx: &Cx, r: &Resource) -> Result<()> {
        match r {
            Resource::File(f) => {
                cx.check_path(&cx.resolve(&f.path)?).with_context(|| f.path.clone())?;
                check_file(cx, f).with_context(|| f.path.clone())
            }
            Resource::Secret(s) => {
                cx.check_path(&cx.resolve(&s.path)?).with_context(|| s.path.clone())?;
                parse_perms(&s.perms).with_context(|| s.path.clone())?;
                require_src(cx, &s.src).with_context(|| s.path.clone())
            }
            _ => Ok(()),
        }
    }

    fn drift(&self, cx: &Cx, entry: &Entry) -> Result<Vec<String>> {
        let Some(path) = target_path(cx, &entry.resource)? else { return Ok(vec![]) };
        if let Resource::File(f) = &entry.resource
            && f.mode == FileMode::Absent
        {
            return Ok(fs::symlink_metadata(&path)
                .is_ok()
                .then(|| "it exists again".to_string())
                .into_iter()
                .collect());
        }
        let mut notes = drift_notes(&entry.resource, &entry.record, &read_live(&path)?);
        let recorded = live::recorded(&entry.record, "sha256");
        match &entry.resource {
            Resource::File(f) if f.mode == FileMode::Managed => match render_bytes(cx, f) {
                Ok(bytes) if recorded.is_some_and(|r| r != sha256(&bytes)) => notes.push("source changed".into()),
                Ok(_) => {}
                Err(e) => notes.push(format!("source unreadable: {e:#}")),
            },
            Resource::Secret(s) => {
                let src = fs::read(cx.spec_dir.join(&s.src)).map(|b| sha256(&b)).ok();
                if src.is_some() && live::recorded(&entry.record, "src_sha256") != src.as_deref() {
                    notes.push("source changed".into());
                }
            }
            _ => {}
        }
        Ok(notes)
    }

    fn same_result(&self, cx: &Cx, entry: &Entry, new: &Resource) -> Result<bool> {
        let Resource::File(f) = new else { return Ok(false) };
        Ok(match f.mode {
            // A seeded file is never written again, whatever its source.
            FileMode::Seed | FileMode::Absent => true,
            FileMode::Managed => {
                render_bytes(cx, f).is_ok_and(|b| live::recorded(&entry.record, "sha256") == Some(sha256(&b).as_str()))
            }
            FileMode::Link | FileMode::Patch => false,
        })
    }

    fn prepare(&self, cx: &Cx, ops: &[Op], staging: &Path) -> Result<()> {
        for op in ops.iter().filter(|o| !o.is_removal()) {
            let staged = staged_path(staging, op);
            match op.resource() {
                Resource::File(f) => match f.mode {
                    FileMode::Managed | FileMode::Seed => stage(&staged, &render_bytes(cx, f)?)?,
                    FileMode::Link => {
                        link_target(cx, f).with_context(|| f.path.clone())?;
                    }
                    FileMode::Patch => {
                        patch_result(cx, op, f).with_context(|| f.path.clone())?;
                    }
                    FileMode::Absent => {}
                },
                Resource::Secret(s) => {
                    let ids = secret::identity_paths(cx.paths, cx.scope, &s.identities)?;
                    let plain =
                        secret::decrypt_file(&cx.spec_dir.join(&s.src), &ids).with_context(|| s.path.clone())?;
                    stage(&staged, &plain)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn commit(&self, cx: &Cx, ops: &[Op], staging: &Path, journal: &mut Journal) -> Result<Records> {
        let mut records = Records::new();
        // A unit file written as a plain file (one an app's installer would write) is only seen
        // by systemd after a reload. Undo replays backwards, so the reload recorded first runs
        // after the files are restored.
        let reload = (cx.facts.init == Init::Systemd && ops.iter().any(|op| is_unit_file(op.resource())))
            .then(|| Cmd::new("systemctl").args((cx.scope == Scope::User).then_some("--user")).arg("daemon-reload"));
        if let Some(cmd) = &reload {
            journal.record(Undo::Run { cmd: cmd.clone() })?;
        }
        for op in ops {
            let record = match op.resource() {
                Resource::File(f) => commit_file(cx, op, f, staging, journal).with_context(|| f.path.clone())?,
                Resource::Secret(s) => commit_secret(cx, op, s, staging, journal).with_context(|| s.path.clone())?,
                _ => continue,
            };
            if let Some(record) = record {
                records.insert(op.key.clone(), record);
            }
        }
        if let Some(cmd) = &reload {
            // No manager answers inside a chroot or before the user's first login.
            cx.runner.run(cmd)?;
        }
        Ok(records)
    }

    /// Links `commit` held back for a missing target, now that every phase has run. One whose
    /// target is still missing is linked dangling, as the spec asked.
    fn settle(&self, cx: &Cx, ops: &[Op], journal: &mut Journal) -> Result<()> {
        for op in ops {
            if let Resource::File(f) = op.resource()
                && f.mode == FileMode::Link
                && f.target.is_some()
            {
                let path = cx.resolve(&f.path)?;
                commit_link(cx, op, f, &path, journal).with_context(|| f.path.clone())?;
            }
        }
        Ok(())
    }
}

fn is_unit_file(r: &Resource) -> bool {
    let Resource::File(f) = r else { return false };
    ["/etc/systemd/system/", "/etc/systemd/user/", "~/.config/systemd/user/"].iter().any(|d| f.path.starts_with(d))
}

fn target_path(cx: &Cx, r: &Resource) -> Result<Option<PathBuf>> {
    match r {
        Resource::File(f) => cx.resolve(&f.path).map(Some),
        Resource::Secret(s) => cx.resolve(&s.path).map(Some),
        _ => Ok(None),
    }
}

fn parse_perms(s: &str) -> Result<u32> {
    u32::from_str_radix(s.trim_start_matches("0o"), 8)
        .ok()
        .filter(|m| *m <= 0o7777)
        .with_context(|| format!("perms `{s}` is not an octal mode like \"0644\""))
}

fn require_src(cx: &Cx, src: &str) -> Result<()> {
    let path = cx.spec_dir.join(src);
    if !path.is_file() {
        bail!("src {} does not exist", path.display());
    }
    Ok(())
}

/// A link points into the repo (`src`, a file or a directory) or outside it (`target`).
fn link_target(cx: &Cx, f: &File) -> Result<PathBuf> {
    match (&f.src, &f.target) {
        (Some(src), None) => {
            let path = cx.checkout.unwrap_or(cx.spec_dir).join(src);
            if !path.exists() {
                bail!("src {} does not exist", path.display());
            }
            Ok(std::path::absolute(path)?)
        }
        (None, Some(t)) => match t.strip_prefix("~/") {
            Some(rest) => Ok(cx.paths.home.join(rest)),
            None if t.starts_with('/') => Ok(PathBuf::from(t)),
            None => bail!("target {t} must be absolute or start with ~/"),
        },
        _ => bail!("a link takes exactly one of src and target"),
    }
}

fn check_file(cx: &Cx, f: &File) -> Result<()> {
    if let Some(p) = &f.perms {
        parse_perms(p)?;
    }
    match f.mode {
        FileMode::Managed | FileMode::Seed => {
            match (&f.src, &f.content) {
                (Some(src), None) => require_src(cx, src)?,
                (None, Some(Value::String(_))) => {}
                (None, Some(v)) => {
                    let format = render::format_of(f).context("cannot infer a format from the path; set format")?;
                    render::serialize(v, format)?;
                }
                _ => bail!("set exactly one of src and content"),
            }
            Ok(())
        }
        FileMode::Link => {
            if f.content.is_some() {
                bail!("a link takes src or target, not content");
            }
            link_target(cx, f).map(drop)
        }
        FileMode::Patch => {
            if f.src.is_some() || f.content.is_some() {
                bail!("a patch takes owns, not src or content");
            }
            match render::format_of(f) {
                Some(Format::Json | Format::Toml | Format::Yaml) => {}
                _ => bail!("patch mode edits json, toml and yaml files; set format or use such an extension"),
            }
            for (key, v) in &f.owns {
                patch::Want::parse(key, v)?;
            }
            Ok(())
        }
        FileMode::Absent => {
            if f.src.is_some() || f.content.is_some() || f.target.is_some() || !f.owns.is_empty() || f.perms.is_some() {
                bail!("an absent file takes only its path");
            }
            Ok(())
        }
    }
}

pub(crate) fn render_bytes(cx: &Cx, f: &File) -> Result<Vec<u8>> {
    match (&f.src, &f.content) {
        (Some(src), None) => {
            let path = cx.spec_dir.join(src);
            fs::read(&path).with_context(|| format!("reading {}", path.display()))
        }
        (None, Some(Value::String(s))) => Ok(s.clone().into_bytes()),
        (None, Some(v)) => {
            let format = render::format_of(f).context("cannot infer a format from the path; set format")?;
            Ok(render::serialize(v, format)?.into_bytes())
        }
        _ => bail!("set exactly one of src and content"),
    }
}

fn staged_path(staging: &Path, op: &Op) -> PathBuf {
    staging.join("files").join(&sha256(op.key.to_string().as_bytes())[..16])
}

fn stage(path: &Path, bytes: &[u8]) -> Result<()> {
    fs::create_dir_all(path.parent().context("staged path has no parent")?)?;
    write_private(path, bytes)
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("writing {}", path.display()))?;
    file.write_all(bytes)?;
    Ok(())
}

/// Write-then-rename `bytes` over `dest`, journaling the previous content first. The temporary
/// file is created 0600 so a secret is never world-readable, even briefly.
fn install(journal: &mut Journal, dest: &Path, bytes: &[u8], mode: Option<u32>) -> Result<()> {
    clear_dangling(journal, dest)?;
    journal.backup(dest)?;
    write_atomic(dest, bytes, mode)
}

/// Swap each dangling symlink among `path`'s ancestors for a directory, journaled. A home shared
/// with another root keeps that root's links, such as home-manager's into a `/nix/store` this
/// root does not have, and `create_dir_all` fails on them.
fn clear_dangling(journal: &mut Journal, path: &Path) -> Result<()> {
    let mut dirs: Vec<&Path> = path.ancestors().skip(1).collect();
    dirs.reverse();
    for dir in dirs {
        if fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_symlink()) && !dir.exists() {
            journal.backup(dir)?;
            remove_any(dir)?;
            fs::create_dir(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
    }
    Ok(())
}

fn write_atomic(dest: &Path, bytes: &[u8], mode: Option<u32>) -> Result<()> {
    let dir = dest.parent().context("path has no parent")?;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let name = dest.file_name().context("path has no file name")?.to_string_lossy();
    let tmp = dir.join(format!(".{name}.fspec-tmp"));
    let mode = mode.or_else(|| fs::metadata(dest).ok().map(|m| m.permissions().mode() & 0o7777)).unwrap_or(0o644);
    let result = write_private(&tmp, bytes)
        .and_then(|()| fs::set_permissions(&tmp, fs::Permissions::from_mode(mode)).map_err(Into::into))
        .and_then(|()| fs::rename(&tmp, dest).with_context(|| format!("replacing {}", dest.display())));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn warn_kept(path: &Path) {
    eprintln!("fspec: keeping {}: it changed since fspec wrote it", path.display());
}

fn hash_record(bytes: &[u8]) -> Map<String, Value> {
    let mut rec = Map::new();
    rec.insert("sha256".into(), sha256(bytes).into());
    rec
}

fn commit_file(
    cx: &Cx,
    op: &Op,
    f: &File,
    staging: &Path,
    journal: &mut Journal,
) -> Result<Option<Map<String, Value>>> {
    let path = cx.resolve(&f.path)?;
    if op.is_removal() {
        remove_file_resource(cx, op, f.mode, &path, journal)?;
        return Ok(None);
    }
    let staged = staged_path(staging, op);
    match f.mode {
        FileMode::Managed => {
            let live = read_live(&path)?;
            let drifted = !live::drift_notes(op.resource(), &op.record(), &live).is_empty();
            if f.on_drift == OnDrift::Fail && drifted {
                bail!(
                    "{} was changed outside fspec; run `fspec adopt {}` to keep the change, or set on_drift = \"overwrite\"",
                    path.display(),
                    f.path
                );
            }
            let bytes = fs::read(&staged)?;
            install(journal, &path, &bytes, file_mode(cx, f)?)?;
            Ok(Some(hash_record(&bytes)))
        }
        FileMode::Seed => {
            if fs::symlink_metadata(&path).is_ok() {
                return Ok(Some(op.record()));
            }
            let bytes = fs::read(&staged)?;
            install(journal, &path, &bytes, file_mode(cx, f)?)?;
            Ok(Some(hash_record(&bytes)))
        }
        // The target comes from a package or service later phases install (resolv.conf onto
        // resolved's stub); linking now would cut what those phases need. `settle` links it.
        FileMode::Link if awaits_target(cx, f)? => Ok(Some(link_record(&link_target(cx, f)?, f))),
        FileMode::Link => commit_link(cx, op, f, &path, journal).map(Some),
        FileMode::Patch => {
            let (text, record) = patch_result(cx, op, f)?;
            if let Some(text) = text {
                install(journal, &path, text.as_bytes(), None)?;
            }
            Ok(Some(record))
        }
        FileMode::Absent => {
            match fs::symlink_metadata(&path) {
                Err(_) => {}
                Ok(m) if m.is_dir() => bail!("{} is a directory; absent only removes a file or a link", path.display()),
                Ok(_) => {
                    journal.backup(&path)?;
                    remove_any(&path)?;
                }
            }
            Ok(Some(Map::new()))
        }
    }
}

fn file_mode(cx: &Cx, f: &File) -> Result<Option<u32>> {
    if let Some(p) = &f.perms {
        return parse_perms(p).map(Some);
    }
    Ok(f.src.as_ref().and_then(|s| fs::metadata(cx.spec_dir.join(s)).ok()).map(|m| m.permissions().mode() & 0o7777))
}

fn commit_link(cx: &Cx, op: &Op, f: &File, path: &Path, journal: &mut Journal) -> Result<Map<String, Value>> {
    let target = link_target(cx, f)?;
    // A directory an app made before the link existed holds its own data; the trash keeps it
    // and a rollback puts it back.
    if fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) {
        fspec_core::trash::trash_paths(cx, journal, None, &[path.to_path_buf()])?;
    }
    let old = op.record();
    let into_repo = f.src.is_some() && live::recorded(&old, "src").is_some();
    if let (true, Some(prev), Live::File(_)) = (into_repo, live::recorded(&old, "target"), read_live(path)?)
        && Path::new(prev).is_file()
    {
        // The app replaced our symlink with a file; its content belongs in the repo.
        let prev = Path::new(prev);
        journal.backup(prev)?;
        fs::copy(path, prev).with_context(|| format!("copying {} back to {}", path.display(), prev.display()))?;
    }
    let already = matches!(read_live(path)?, Live::Link(t) if t == target);
    if !already {
        clear_dangling(journal, path)?;
        journal.backup(path)?;
        let dir = path.parent().context("path has no parent")?;
        fs::create_dir_all(dir)?;
        let name = path.file_name().context("path has no file name")?.to_string_lossy();
        let tmp = dir.join(format!(".{name}.fspec-tmp"));
        remove_any(&tmp)?;
        std::os::unix::fs::symlink(&target, &tmp)?;
        fs::rename(&tmp, path).with_context(|| format!("linking {}", path.display()))?;
    }
    Ok(link_record(&target, f))
}

fn link_record(target: &Path, f: &File) -> Map<String, Value> {
    let mut rec = Map::new();
    rec.insert("target".into(), target.to_string_lossy().into_owned().into());
    if let Some(src) = &f.src {
        rec.insert("src".into(), src.as_str().into());
    }
    rec
}

/// A link to a `target` outside the repo that does not exist yet.
fn awaits_target(cx: &Cx, f: &File) -> Result<bool> {
    match (&f.mode, &f.target) {
        (FileMode::Link, Some(t)) => Ok(!cx.resolve(t)?.exists()),
        _ => Ok(false),
    }
}

/// Remove what fspec wrote, but only while it still is what fspec wrote.
fn remove_file_resource(cx: &Cx, op: &Op, mode: FileMode, path: &Path, journal: &mut Journal) -> Result<()> {
    if mode == FileMode::Absent {
        return Ok(());
    }
    let record = op.record();
    let live = read_live(path)?;
    let remove = match (mode, &live) {
        (_, Live::Missing) => return Ok(()),
        (FileMode::Patch, _) => return retract_patch(cx, op, journal),
        (FileMode::Link, Live::Link(t)) => {
            Some(t.as_path() == Path::new(live::recorded(&record, "target").unwrap_or_default()))
        }
        (FileMode::Link, _) => Some(false),
        (_, Live::File(bytes)) => live::recorded(&record, "sha256").map(|h| h == sha256(bytes)),
        (_, Live::Link(_)) => Some(false),
    };
    match remove {
        Some(true) => {
            journal.backup(path)?;
            remove_any(path)
        }
        Some(false) => {
            warn_kept(path);
            Ok(())
        }
        None => Ok(()),
    }
}

fn retract_patch(cx: &Cx, op: &Op, journal: &mut Journal) -> Result<()> {
    let Resource::File(f) = op.resource() else { return Ok(()) };
    let path = cx.resolve(&f.path)?;
    if let (Some(text), _) = patch_result(cx, op, f)? {
        install(journal, &path, text.as_bytes(), None)?;
    }
    Ok(())
}

/// The new text of a patched file, `None` when nothing changes, and the record to keep.
fn patch_result(cx: &Cx, op: &Op, f: &File) -> Result<(Option<String>, Map<String, Value>)> {
    let path = cx.resolve(&f.path)?;
    let format = render::format_of(f).context("patch mode needs a format")?;
    let doc = match fs::read_to_string(&path) {
        Ok(text) => render::parse(&text, format).with_context(|| format!("parsing {}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Value::Object(Default::default()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let (old_owns, record) = match &op.current {
        Some(Entry { resource: Resource::File(old), record, .. }) => (old.owns.clone(), Record::from_map(record)),
        _ => (Default::default(), Record::default()),
    };
    let owns = if op.is_removal() { Default::default() } else { f.owns.clone() };
    let merged = patch::merge(&doc, &owns, &old_owns, &record, f.on_conflict)?;
    if !merged.conflicts.is_empty() {
        bail!(
            "{} changed since fspec last wrote {} and on_conflict is \"fail\"",
            path.display(),
            merged.conflicts.iter().map(|k| format!("`{k}`")).collect::<Vec<_>>().join(", ")
        );
    }
    let text = if merged.changed { Some(render::serialize(&merged.doc, format)?) } else { None };
    Ok((text, merged.record.to_map()))
}

fn commit_secret(
    cx: &Cx,
    op: &Op,
    s: &Secret,
    staging: &Path,
    journal: &mut Journal,
) -> Result<Option<Map<String, Value>>> {
    let path = cx.resolve(&s.path)?;
    if op.is_removal() {
        remove_file_resource(cx, op, FileMode::Managed, &path, journal)?;
        return Ok(None);
    }
    let plain = fs::read(staged_path(staging, op))?;
    install(journal, &path, &plain, Some(parse_perms(&s.perms)?))?;
    if let Some(owner) = &s.owner {
        cx.runner.check(&Cmd::new("chown").arg(owner).arg(path.to_string_lossy()))?;
    }
    // A copy of the ciphertext lets `fspec secret install` decrypt again at boot, since the
    // plaintext usually lives on a tmpfs.
    let ciphertext = fs::read(cx.spec_dir.join(&s.src))?;
    let src_sha = sha256(&ciphertext);
    let kept = fspec_core::apply::state_dir(cx.paths, cx.scope).join("secrets").join(format!("{src_sha}.age"));
    if !kept.exists() {
        install(journal, &kept, &ciphertext, Some(0o600))?;
    }
    let ids = secret::identity_paths(cx.paths, cx.scope, &s.identities)?;
    let mut rec = hash_record(&plain);
    rec.insert("src_sha256".into(), src_sha.into());
    rec.insert("identities".into(), ids.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>().into());
    Ok(Some(rec))
}

/// Decrypt every secret in `state` again from its kept ciphertext, for a boot that wiped the
/// tmpfs they live on. Returns the paths written.
pub fn reinstall_secrets(
    paths: &fspec_core::paths::Paths,
    scope: fspec_core::resource::Scope,
    state: &fspec_core::state::State,
    runner: &dyn fspec_core::runner::Runner,
) -> Result<Vec<PathBuf>> {
    let dir = fspec_core::apply::state_dir(paths, scope).join("secrets");
    let mut written = Vec::new();
    for entry in state.entries.values() {
        let Resource::Secret(s) = &entry.resource else { continue };
        let sha =
            live::recorded(&entry.record, "src_sha256").with_context(|| format!("{}: no recorded source", s.path))?;
        let ids: Vec<PathBuf> = entry
            .record
            .get("identities")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(PathBuf::from).collect())
            .unwrap_or_default();
        let ciphertext = fs::read(dir.join(format!("{sha}.age")))
            .with_context(|| format!("{}: the kept ciphertext is missing", s.path))?;
        let plain = secret::decrypt(&ciphertext, &secret::load_identities(&ids)?).with_context(|| s.path.clone())?;
        let path = paths.resolve(&s.path)?;
        write_atomic(&path, &plain, Some(parse_perms(&s.perms)?)).with_context(|| s.path.clone())?;
        if let Some(owner) = &s.owner {
            runner.check(&Cmd::new("chown").arg(owner).arg(path.to_string_lossy()))?;
        }
        written.push(path);
    }
    Ok(written)
}

#[cfg(test)]
mod tests;
