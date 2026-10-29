use std::fs;
use std::io::{Cursor, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use fspec_core::journal::remove_any;
use fspec_core::runner::{Cmd, Runner};
use sha2::{Digest, Sha256, Sha512};

/// `sha256-<base64>` of `body`, the form `fspec.lock` stores.
pub fn sri_sha256(body: &[u8]) -> String {
    format!("sha256-{}", STANDARD.encode(Sha256::digest(body)))
}

/// Check `body` against an SRI string (`sha256-…` or `sha512-…`).
pub fn verify(body: &[u8], sri: &str) -> Result<()> {
    let (algo, want) = sri.split_once('-').with_context(|| format!("hash `{sri}` is not <algo>-<base64>"))?;
    let got = match algo {
        "sha256" => STANDARD.encode(Sha256::digest(body)),
        "sha512" => STANDARD.encode(Sha512::digest(body)),
        other => bail!("hash algorithm `{other}` is not supported, use sha256 or sha512"),
    };
    if got.trim_end_matches('=') != want.trim_end_matches('=') {
        bail!("hash mismatch: the lock has {sri}, the download is {algo}-{got}");
    }
    Ok(())
}

/// Whether `body` is a format [`unpack`] opens: gzip, xz, zstd or zip by magic bytes, an Electron
/// asar, or a brotli-compressed tar (brotli has no magic, so the body is decoded and checked).
pub fn is_archive(body: &[u8]) -> bool {
    [&[0x1f, 0x8b][..], &[0xfd, b'7', b'z', b'X', b'Z', 0], &[0x28, 0xb5, 0x2f, 0xfd], b"PK\x03\x04"]
        .iter()
        .any(|magic| body.starts_with(magic))
        || is_asar(body)
        || is_dmg(body)
        || (!is_elf(body) && brotli_tar(body).is_some())
}

pub fn is_elf(body: &[u8]) -> bool {
    body.starts_with(b"\x7fELF")
}

pub fn unpack(body: &[u8], dest: &Path, name: &str) -> Result<()> {
    fs::create_dir_all(dest).with_context(|| format!("creating {}", dest.display()))?;
    // A compressed stream holds a tar, or one file such as rust-analyzer's gzipped executable.
    let stream = if body.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(body).read_to_end(&mut out).context("unpacking gzip")?;
        Some(out)
    } else if body.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0]) {
        let mut out = Vec::new();
        lzma_rs::xz_decompress(&mut &body[..], &mut out).map_err(|e| anyhow!("unpacking xz: {e}"))?;
        Some(out)
    } else if body.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        let mut out = Vec::new();
        ruzstd::decoding::StreamingDecoder::new(body)
            .map_err(|e| anyhow!("unpacking zstd: {e}"))?
            .read_to_end(&mut out)
            .context("unpacking zstd")?;
        Some(out)
    } else {
        None
    };
    if let Some(inner) = stream {
        if is_tar(&inner) { untar(&inner[..], dest) } else { write_command(&inner, dest, name) }
    } else if body.starts_with(b"PK\x03\x04") {
        zip::ZipArchive::new(Cursor::new(body)).and_then(|mut z| z.extract(dest)).context("unpacking zip")
    } else if is_asar(body) {
        unasar(body, dest).context("unpacking asar")
    } else if let Some(tar) = (!is_elf(body)).then(|| brotli_tar(body)).flatten() {
        untar(&tar[..], dest)
    } else {
        write_command(body, dest, name)
    }
}

/// A UDIF disk image ends with a 512-byte `koly` trailer.
pub fn is_dmg(body: &[u8]) -> bool {
    body.len() >= 512 && body[body.len() - 512..].starts_with(b"koly")
}

/// Mount the image read-only beside `dest`, ditto its contents out (ditto keeps the bundles'
/// extended attributes and signatures), then drop the `Applications` link and the hidden
/// background files a drag-to-install window has.
pub fn undmg(runner: &dyn Runner, body: &[u8], dest: &Path) -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("a .dmg unpacks only on macOS");
    }
    let image = dest.with_extension("dmg");
    let mount = dest.with_extension("mnt");
    // The image sits beside `dest`, whose parent no other package may have made yet.
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&image, body)?;
    fs::create_dir_all(&mount)?;
    let attach = Cmd::new("hdiutil")
        .args(["attach", "-nobrowse", "-readonly", "-noautoopen", "-mountpoint"])
        .arg(mount.display().to_string())
        .arg(image.display().to_string());
    let copied = runner.check(&attach).and_then(|_| {
        let copied = runner.check(&Cmd::new("ditto").arg(mount.display().to_string()).arg(dest.display().to_string()));
        let _ = runner.run(&Cmd::new("hdiutil").args(["detach", "-quiet"]).arg(mount.display().to_string()));
        copied
    });
    let _ = fs::remove_dir(&mount);
    let _ = fs::remove_file(&image);
    copied?;
    for entry in fs::read_dir(dest).into_iter().flatten().flatten() {
        if entry.file_name().to_string_lossy().starts_with('.') || entry.file_type()?.is_symlink() {
            remove_any(&entry.path())?;
        }
    }
    Ok(())
}

fn is_tar(bytes: &[u8]) -> bool {
    bytes.len() > 262 && &bytes[257..262] == b"ustar"
}

fn write_command(bytes: &[u8], dest: &Path, name: &str) -> Result<()> {
    let path = dest.join(name);
    fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

/// The tar inside a brotli stream, as Discord ships its `full.distro` builds.
fn brotli_tar(body: &[u8]) -> Option<Vec<u8>> {
    let mut tar = Vec::new();
    brotli_decompressor::Decompressor::new(body, 64 * 1024).read_to_end(&mut tar).ok()?;
    is_tar(&tar).then_some(tar)
}

/// An Electron asar starts with a pickle holding the header size, then a pickle holding the JSON
/// file table.
fn is_asar(body: &[u8]) -> bool {
    body.len() > 16 && body[..4] == [4, 0, 0, 0] && body[16..].starts_with(b"{\"files\"")
}

/// Unpack an asar the way moonlight's own extractor reads one: the JSON table maps names to
/// `{"files": ...}` directories or `{"size", "offset"}` files, offsets counted from the end of
/// the header pickle.
fn unasar(body: &[u8], dest: &Path) -> Result<()> {
    let u32_at = |at: usize| -> Result<usize> {
        let b = body.get(at..at + 4).context("asar header is truncated")?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
    };
    let header_pickle = u32_at(8)?;
    let json_len = u32_at(12)?;
    let json = body.get(16..16 + json_len).context("asar header is truncated")?;
    let table: serde_json::Value = serde_json::from_slice(json).context("parsing the asar header")?;
    let base = 12 + header_pickle;
    fn walk(body: &[u8], base: usize, dir: &serde_json::Value, at: &Path) -> Result<()> {
        let files = dir.get("files").and_then(|f| f.as_object()).context("asar directory has no files")?;
        fs::create_dir_all(at)?;
        for (name, entry) in files {
            if name.is_empty() || name == "." || name == ".." || name.contains('/') {
                bail!("asar entry `{name}` leaves its directory");
            }
            let path = at.join(name);
            if entry.get("files").is_some() {
                walk(body, base, entry, &path)?;
                continue;
            }
            if entry.get("unpacked").and_then(|u| u.as_bool()) == Some(true) || entry.get("link").is_some() {
                bail!("asar entry `{name}` lives outside the archive");
            }
            let size = entry.get("size").and_then(|s| s.as_u64()).context("asar file has no size")? as usize;
            let offset: usize = entry
                .get("offset")
                .and_then(|o| o.as_str())
                .and_then(|o| o.parse().ok())
                .context("asar file has no offset")?;
            let start = base + offset;
            let data = body.get(start..start + size).with_context(|| format!("asar file `{name}` is truncated"))?;
            fs::write(&path, data)?;
            if entry.get("executable").and_then(|e| e.as_bool()) == Some(true) {
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
            }
        }
        Ok(())
    }
    walk(body, base, &table, dest)
}

fn untar(reader: impl Read, dest: &Path) -> Result<()> {
    let mut archive = tar::Archive::new(reader);
    archive.set_preserve_permissions(true);
    archive.unpack(dest).context("unpacking tar")
}

/// First eight hex digits of the SHA-256 of the artifact's SRI string.
pub fn hash8(sri: &str) -> String {
    Sha256::digest(sri.as_bytes()).iter().take(4).map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn is_exec(p: &Path) -> bool {
        fs::metadata(p).unwrap().permissions().mode() & 0o111 != 0
    }

    #[test]
    fn verifies_both_hash_algorithms() {
        let body = b"payload";
        verify(body, &sri_sha256(body)).unwrap();
        let sha512 = format!("sha512-{}", STANDARD.encode(Sha512::digest(body)));
        verify(body, &sha512).unwrap();
        let err = verify(b"other", &sri_sha256(body)).unwrap_err();
        assert!(err.to_string().contains("hash mismatch"), "{err}");
        assert!(verify(body, "md5-AAAA").is_err());
    }

    #[test]
    fn a_gzipped_executable_becomes_the_command() {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(b"\x7fELF tool").unwrap();
        let dir = tempfile::tempdir().unwrap();
        unpack(&gz.finish().unwrap(), dir.path(), "rust-analyzer").unwrap();
        assert_eq!(fs::read(dir.path().join("rust-analyzer")).unwrap(), b"\x7fELF tool");
        assert!(is_exec(&dir.path().join("rust-analyzer")));
    }

    #[test]
    fn unpacks_xz_zstd_zip_and_raw() {
        let xz = include_bytes!("../tests/fixtures/hello.tar.xz");
        let zst = include_bytes!("../tests/fixtures/hello.tar.zst");
        for (label, body) in [("xz", &xz[..]), ("zst", &zst[..])] {
            let dir = tempfile::tempdir().unwrap();
            unpack(body, dir.path(), "hello").unwrap();
            assert!(is_exec(&dir.path().join("hello-1.0.0/hello")), "{label}");
        }

        let mut zip_bytes = Vec::new();
        {
            let mut z = zip::ZipWriter::new(Cursor::new(&mut zip_bytes));
            let opts = zip::write::SimpleFileOptions::default().unix_permissions(0o755);
            z.start_file("hello", opts).unwrap();
            z.write_all(b"#!/bin/sh\n").unwrap();
            z.finish().unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        unpack(&zip_bytes, dir.path(), "hello").unwrap();
        assert!(is_exec(&dir.path().join("hello")));

        let dir = tempfile::tempdir().unwrap();
        unpack(b"\x7fELF raw", dir.path(), "tool").unwrap();
        assert!(is_exec(&dir.path().join("tool")));
    }

    #[test]
    fn unpacks_a_brotli_tar() {
        let br = include_bytes!("../tests/fixtures/hello.tar.br");
        assert!(is_archive(br));
        let dir = tempfile::tempdir().unwrap();
        unpack(br, dir.path(), "hello").unwrap();
        assert!(is_exec(&dir.path().join("hello-1.0.0/hello")));
        assert!(!is_archive(b"plain text that is no brotli stream"));
    }

    /// An asar as `asar pack` writes it: two pickles, then the file bodies.
    fn asar(table: &str, data: &[u8]) -> Vec<u8> {
        let padded = table.len().div_ceil(4) * 4;
        let mut out = Vec::new();
        out.extend(4u32.to_le_bytes());
        out.extend((padded as u32 + 8).to_le_bytes());
        out.extend((padded as u32 + 4).to_le_bytes());
        out.extend((table.len() as u32).to_le_bytes());
        out.extend(table.as_bytes());
        out.resize(16 + padded, 0);
        out.extend(data);
        out
    }

    #[test]
    fn unpacks_an_asar() {
        let table = r#"{"files":{"manifest.json":{"size":2,"offset":"0"},"bin":{"files":{"run":{"size":3,"offset":"2","executable":true}}}}}"#;
        let body = asar(table, b"{}sh!");
        assert!(is_archive(&body));
        let dir = tempfile::tempdir().unwrap();
        unpack(&body, dir.path(), "ext").unwrap();
        assert_eq!(fs::read(dir.path().join("manifest.json")).unwrap(), b"{}");
        assert_eq!(fs::read(dir.path().join("bin/run")).unwrap(), b"sh!");
        assert!(is_exec(&dir.path().join("bin/run")));

        let escape = asar(r#"{"files":{"..":{"files":{}}}}"#, b"");
        assert!(unpack(&escape, tempfile::tempdir().unwrap().path(), "ext").is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_dmg_is_mounted_copied_out_and_pruned() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("store/out");
        // Stands in for what ditto copied out of the mounted image.
        fs::create_dir_all(dest.join("Tool.app/Contents")).unwrap();
        fs::create_dir_all(dest.join(".background")).unwrap();
        std::os::unix::fs::symlink("/Applications", dest.join("Applications")).unwrap();
        let mut body = vec![0u8; 1024];
        body[512..516].copy_from_slice(b"koly");
        assert!(is_archive(&body) && is_dmg(&body));
        let runner = fspec_core::runner::RecordingRunner::default();
        undmg(&runner, &body, &dest).unwrap();
        let (mnt, img, out) = (dest.with_extension("mnt"), dest.with_extension("dmg"), dest.display().to_string());
        assert_eq!(
            runner.lines(),
            [
                format!(
                    "hdiutil attach -nobrowse -readonly -noautoopen -mountpoint {} {}",
                    mnt.display(),
                    img.display()
                ),
                format!("ditto {} {out}", mnt.display()),
                format!("hdiutil detach -quiet {}", mnt.display()),
            ]
        );
        assert!(dest.join("Tool.app").is_dir());
        assert!(fs::symlink_metadata(dest.join("Applications")).is_err() && !dest.join(".background").exists());
        assert!(!mnt.exists() && !img.exists());
    }
}
