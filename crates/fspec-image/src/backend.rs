use std::path::Path;

use anyhow::{Context, Result, bail};
use fspec_core::apply::{Backend, Cx, Op, Phase, Records};
use fspec_core::facts::Os;
use fspec_core::journal::{Journal, Undo};
use fspec_core::resource::Resource;
use fspec_core::runner::Cmd;
use fspec_core::state::Entry;
use serde_json::{Map, Value, json};

use crate::containerfile;

/// Points the host at the image digest in `fspec.lock` with `bootc switch`. The new deployment
/// is staged; it becomes the running system at the next reboot.
pub struct ImageBackend;

/// The locked `repo@sha256:…` for this host.
fn locked<'a>(cx: &Cx<'a>) -> Result<&'a str> {
    let host = &cx.facts.name;
    let Some(l) = cx.lock.system.get(host) else {
        bail!(
            "fspec.lock has no image for host {host}; run `fspec ci init`, let the workflow push the image, \
             then `fspec update --images`"
        )
    };
    match l.image.split_once("@sha256:") {
        Some((repo, hex)) if !repo.is_empty() && !hex.is_empty() => Ok(&l.image),
        _ => bail!("fspec.lock image for host {host} is `{}`, expected <repo>@sha256:<digest>", l.image),
    }
}

fn digest(image: &str) -> &str {
    image.rsplit_once('@').map_or(image, |(_, d)| d)
}

/// The digest the next boot lands on (staged, else booted) from `bootc status --json`. `None`
/// when bootc printed nothing, which is what a rooted apply answers.
fn live_digest(cx: &Cx) -> Result<Option<String>> {
    let out = cx.runner.run(&Cmd::new("bootc").args(["status", "--json"]))?;
    let text = out.stdout.trim();
    if out.status != 0 || text.is_empty() {
        return Ok(None);
    }
    let status: Value = serde_json::from_str(text).context("parsing `bootc status --json`")?;
    let pick = |slot: &str| status["status"][slot]["image"]["imageDigest"].as_str().map(str::to_string);
    Ok(pick("staged").or_else(|| pick("booted")))
}

impl Backend for ImageBackend {
    fn name(&self) -> &'static str {
        "image"
    }

    fn handles(&self, cx: &Cx, r: &Resource) -> bool {
        cx.facts.os == Os::Linux && matches!(r, Resource::Image(_))
    }

    fn phase(&self) -> Phase {
        Phase::System
    }

    fn check(&self, cx: &Cx, r: &Resource) -> Result<()> {
        let Resource::Image(i) = r else { return Ok(()) };
        containerfile(i)?;
        locked(cx).map(drop)
    }

    fn drift(&self, cx: &Cx, entry: &Entry) -> Result<Vec<String>> {
        let want = locked(cx)?;
        let notes = match live_digest(cx)? {
            Some(live) if live != digest(want) => vec![format!("running {live}, lock wants {}", digest(want))],
            Some(_) => vec![],
            None => match entry.record.get("image").and_then(Value::as_str) {
                Some(applied) if applied != want => vec![format!("lock moved to {}", digest(want))],
                _ => vec![],
            },
        };
        Ok(notes)
    }

    fn commit(&self, cx: &Cx, ops: &[Op], _staging: &Path, journal: &mut Journal) -> Result<Records> {
        let mut records = Records::new();
        for op in ops {
            if op.is_removal() {
                continue;
            }
            let want = locked(cx)?;
            if live_digest(cx)?.as_deref() != Some(digest(want)) {
                journal.record(Undo::Run { cmd: Cmd::new("bootc").arg("rollback") })?;
                cx.runner.check(&Cmd::new("bootc").arg("switch").arg(want))?;
                println!("{want} is staged: reboot to activate it");
            }
            let mut record = Map::new();
            record.insert("image".into(), json!(want));
            records.insert(op.key.clone(), record);
        }
        Ok(records)
    }
}

#[cfg(test)]
mod tests {
    use fspec_core::Facts;
    use fspec_core::Lock;
    use fspec_core::Paths;
    use fspec_core::Scope;
    use fspec_core::State;
    use fspec_core::apply::{apply_scope, plan_scope, state_dir};
    use fspec_core::fetch::MapFetch;
    use fspec_core::lock::LockedSystem;
    use fspec_core::plan::Action;
    use fspec_core::resource::{Desired, Image, System};
    use fspec_core::runner::{Output, RecordingRunner};

    use super::*;

    const OLD: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
    const NEW: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";

    struct World {
        _dir: tempfile::TempDir,
        paths: Paths,
        facts: Facts,
        lock: Lock,
        runner: RecordingRunner,
        fetch: MapFetch,
    }

    fn image() -> Image {
        Image {
            base: "ghcr.io/ublue-os/bazzite:stable".into(),
            packages: None,
            kernel_args: vec![],
            distro: None,
            ..Default::default()
        }
    }

    impl World {
        fn new(platform: &str, locked: Option<&str>) -> World {
            let dir = tempfile::tempdir().unwrap();
            let paths = Paths::new(dir.path().into(), dir.path().join("home/u"));
            let system = (platform == "x86_64-linux").then(|| System::Image(image()));
            let facts = Facts::declared("g815", platform, system.as_ref()).unwrap();
            let mut lock = Lock::default();
            if let Some(d) = locked {
                lock.system.insert("g815".into(), LockedSystem { image: format!("ghcr.io/o/fspec-g815@{d}") });
            }
            World { _dir: dir, paths, facts, lock, runner: RecordingRunner::default(), fetch: MapFetch::default() }
        }

        fn linux(locked: Option<&str>) -> World {
            World::new("x86_64-linux", locked)
        }

        fn cx(&self) -> Cx<'_> {
            let state = State::load(&state_dir(&self.paths, Scope::System).join("state.json")).unwrap();
            Cx {
                paths: &self.paths,
                facts: &self.facts,
                spec_dir: &self.paths.root,
                lock: &self.lock,
                runner: &self.runner,
                fetch: &self.fetch,
                generation: state.generation + 1,
                scope: Scope::System,
                protect: &[],
                builders: &[],
                build_cache: None,
                checkout: None,
            }
        }

        fn desired() -> Vec<Desired> {
            vec![Desired { resource: Resource::Image(image()), owner: None }]
        }

        fn apply(&self) -> Result<fspec_core::Plan> {
            apply_scope(&self.cx(), &[&ImageBackend], &World::desired())
        }

        fn plan(&self) -> fspec_core::Plan {
            let state = State::load(&state_dir(&self.paths, Scope::System).join("state.json")).unwrap();
            plan_scope(&self.cx(), &[&ImageBackend], &World::desired(), &state).unwrap()
        }

        fn booted(&self, booted: &str, staged: Option<&str>) {
            let slot =
                |d: &str| json!({"image": {"image": {"image": "ghcr.io/o/fspec-g815:latest"}, "imageDigest": d}});
            let status = json!({"status": {"booted": slot(booted), "staged": staged.map(slot)}});
            self.runner
                .respond("bootc status", Output { status: 0, stdout: status.to_string(), stderr: String::new() });
        }

        fn lines(&self) -> Vec<String> {
            self.runner.lines()
        }
    }

    fn image_ref(d: &str) -> String {
        format!("ghcr.io/o/fspec-g815@{d}")
    }

    #[test]
    fn first_apply_switches_to_the_locked_digest() {
        let w = World::linux(Some(NEW));
        w.apply().unwrap();
        assert_eq!(w.lines(), ["bootc status --json".to_string(), format!("bootc switch {}", image_ref(NEW))]);
        let state = State::load(&state_dir(&w.paths, Scope::System).join("state.json")).unwrap();
        let entry = state.entries.values().next().unwrap();
        assert_eq!(entry.record["image"], image_ref(NEW));
    }

    #[test]
    fn already_running_the_digest_is_a_no_op_switch() {
        let w = World::linux(Some(NEW));
        w.booted(NEW, None);
        w.apply().unwrap();
        assert_eq!(w.lines(), ["bootc status --json"]);
    }

    #[test]
    fn a_staged_digest_counts_as_switched() {
        let w = World::linux(Some(NEW));
        w.booted(OLD, Some(NEW));
        w.apply().unwrap();
        assert_eq!(w.lines(), ["bootc status --json"]);
    }

    #[test]
    fn locked_digest_ahead_of_the_booted_one_is_drift_and_reswitches() {
        let mut w = World::linux(Some(OLD));
        w.booted(OLD, None);
        w.apply().unwrap();
        w.runner.calls.lock().unwrap().clear();
        assert!(w.plan().changes.is_empty());

        w.lock.system.insert("g815".into(), LockedSystem { image: image_ref(NEW) });
        let p = w.plan();
        assert_eq!(p.changes.len(), 1);
        let Action::Drift { notes } = &p.changes[0].action else { panic!("{:?}", p.changes[0]) };
        assert_eq!(notes, &[format!("running {OLD}, lock wants {NEW}")]);

        w.runner.calls.lock().unwrap().clear();
        w.apply().unwrap();
        assert_eq!(
            w.lines(),
            [
                "bootc status --json".to_string(),
                "bootc status --json".to_string(),
                format!("bootc switch {}", image_ref(NEW))
            ]
        );
    }

    #[test]
    fn unknown_live_state_reports_only_a_moved_lock() {
        let mut w = World::linux(Some(OLD));
        w.apply().unwrap();
        assert!(w.plan().changes.is_empty());
        w.lock.system.insert("g815".into(), LockedSystem { image: image_ref(NEW) });
        let Action::Drift { notes } = &w.plan().changes[0].action else { panic!() };
        assert_eq!(notes, &[format!("lock moved to {NEW}")]);
    }

    #[test]
    fn missing_lock_entry_is_listed_in_the_plan_and_blocks_apply() {
        let w = World::linux(None);
        let p = w.plan();
        assert_eq!(p.unsupported.len(), 1);
        for want in ["fspec ci init", "fspec update --images"] {
            assert!(p.unsupported[0].contains(want), "{}", p.unsupported[0]);
        }
        assert!(w.apply().is_err());
        assert!(w.lines().is_empty());
    }

    #[test]
    fn failed_switch_is_journaled_and_rolled_back() {
        let w = World::linux(Some(NEW));
        w.runner.respond("bootc switch", Output { status: 1, stdout: String::new(), stderr: "no".into() });
        assert!(w.apply().is_err());
        assert_eq!(w.lines().last().unwrap(), "bootc rollback");
    }

    #[test]
    fn only_linux_hosts_claim_images() {
        let w = World::new("aarch64-darwin", None);
        assert!(!ImageBackend.handles(&w.cx(), &Resource::Image(image())));
        let w = World::linux(None);
        assert!(ImageBackend.handles(&w.cx(), &Resource::Image(image())));
    }
}
