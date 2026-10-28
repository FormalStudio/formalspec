//! `fspec adopt`: pull an app's edit of a managed file back into the repo.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use fspec_core::Cx;
use fspec_core::resource::{File, FileMode};
use serde_json::{Map, Value};
use similar::TextDiff;

use crate::live::{self, Live, read_live, sha256};
use crate::{hash_record, render_bytes};

#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// The file on disk is what fspec wrote; nothing to pull back.
    InSync,
    /// The on-disk content now lives in the repo at `to`. `record` replaces the state record, so
    /// the file stops reading as drifted.
    Copied { to: PathBuf, record: Map<String, Value> },
    /// The content comes from Starlark, so there is no repo file to update. Edit the spec to
    /// match this diff (spec on the left, disk on the right).
    Generated { diff: String },
    /// A patch owns keys, not a file. These owned keys no longer hold what fspec wrote.
    Patch { notes: Vec<String> },
}

pub fn adopt(cx: &Cx, f: &File, record: &Map<String, Value>) -> Result<Outcome> {
    let path = cx.resolve(&f.path)?;
    let live = read_live(&path)?;
    match f.mode {
        FileMode::Absent => bail!("{} is declared absent, so there is nothing to adopt", f.path),
        FileMode::Seed => bail!("{} is a seed: fspec never manages it after the first write", f.path),
        FileMode::Patch => {
            let notes = live::drift_notes(&fspec_core::Resource::File(f.clone()), record, &live);
            Ok(if notes.is_empty() { Outcome::InSync } else { Outcome::Patch { notes } })
        }
        // Only a link into the repo has a copy there to adopt into; a target outside it is not
        // fspec's to overwrite.
        FileMode::Link => match (live, live::recorded(record, "target").filter(|_| f.src.is_some())) {
            (Live::File(bytes), Some(target)) => {
                fs::write(target, bytes).with_context(|| format!("writing {target}"))?;
                Ok(Outcome::Copied { to: target.into(), record: record.clone() })
            }
            _ => Ok(Outcome::InSync),
        },
        FileMode::Managed => {
            let Live::File(disk) = live else { bail!("{} is not a regular file, nothing to adopt", path.display()) };
            if live::recorded(record, "sha256") == Some(&sha256(&disk)) {
                return Ok(Outcome::InSync);
            }
            let Some(src) = &f.src else {
                let rendered = String::from_utf8_lossy(&render_bytes(cx, f)?).into_owned();
                let diff = TextDiff::from_lines(&rendered, String::from_utf8_lossy(&disk))
                    .unified_diff()
                    .header("spec", "disk")
                    .to_string();
                return Ok(Outcome::Generated { diff });
            };
            let to = cx.spec_dir.join(src);
            fs::write(&to, &disk).with_context(|| format!("writing {}", to.display()))?;
            Ok(Outcome::Copied { to, record: hash_record(&disk) })
        }
    }
}
