use std::fs;
use std::path::Path;

use anyhow::bail;
use fspec_core::Host;
use fspec_core::runner::{Output, RecordingRunner};
use fspec_remote::{ENGINE, Planned, Remote, apply_all, order};
use serde_json::json;

fn host(name: &str, platform: &str, distro: &str) -> Host {
    let system = if platform.ends_with("darwin") {
        json!(null)
    } else {
        json!({"kind": "distro", "distro": distro, "init": "systemd"})
    };
    serde_json::from_value(json!({"name": name, "platform": platform, "system": system})).unwrap()
}

fn ok(stdout: &str) -> Output {
    Output { status: 0, stdout: stdout.into(), stderr: String::new() }
}

fn probe_json(platform: &str, distro: &str) -> String {
    json!({"hostname": "g815", "platform": platform, "distro": distro, "engine": ENGINE}).to_string()
}

const PROBE: &str = "ssh -o ConnectTimeout=5 -o BatchMode=yes g815";

fn fixture(status: &str) -> (tempfile::TempDir, RecordingRunner) {
    with_untracked(status, "")
}

fn with_untracked(status: &str, untracked: &str) -> (tempfile::TempDir, RecordingRunner) {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("fspec.lock"), "").unwrap();
    let runner = RecordingRunner::default();
    let git = format!("git -C {}", dir.path().display());
    runner.respond(&format!("{git} rev-parse"), ok("abc123\n"));
    runner.respond(&format!("{git} status"), ok(status));
    runner.respond(&format!("{git} ls-files --others"), ok(untracked));
    runner.respond(&format!("{git} ls-files"), ok("spec.star\nmodules/cli.star\n"));
    (dir, runner)
}

fn remote<'a>(dir: &Path, runner: &'a RecordingRunner, dirty: bool) -> Remote<'a> {
    Remote { runner, spec_dir: dir.to_path_buf(), spec_file: "spec.star".into(), dirty, loads_cache: None }
}

fn apply_lines(dir: &Path, runner: &RecordingRunner) -> Vec<String> {
    let git = format!("git -C {}", dir.display());
    runner.lines().into_iter().filter(|l| !l.starts_with(&git)).collect()
}

#[test]
fn apply_probes_ships_then_runs_with_a_terminal() {
    let (dir, runner) = fixture("");
    runner.respond(
        &format!("{PROBE} 'PATH=\"/usr/local/bin:$PATH\" fspec facts --json'"),
        ok(&probe_json("x86_64-linux", "bazzite")),
    );
    let h = host("g815", "x86_64-linux", "bazzite");
    remote(dir.path(), &runner, false).apply(&h, "g815", None).unwrap();

    let lines = apply_lines(dir.path(), &runner);
    let tar = lines[1].clone();
    assert!(tar.starts_with("tar -cf ") && tar.ends_with(&format!(" -C {} -T -", dir.path().display())), "{tar}");
    let dest = "\"$HOME/.cache/fspec/spec/abc123\"";
    assert_eq!(lines[0], format!("{PROBE} 'PATH=\"/usr/local/bin:$PATH\" fspec facts --json'"));
    assert_eq!(lines[2], format!("{PROBE} 'mkdir -p {dest} && tar -xf - -C {dest}'"));
    assert_eq!(
        lines[3],
        format!("ssh -t g815 'PATH=\"/usr/local/bin:$PATH\" fspec --spec {dest}/spec.star apply --local --host g815'")
    );
    assert_eq!(lines.len(), 4);

    let calls = runner.calls.lock().unwrap();
    let tar = calls.iter().find(|c| c.program == "tar").unwrap();
    assert_eq!(tar.stdin.as_deref(), Some("spec.star\nmodules/cli.star\nfspec.lock"));
    assert!(calls.last().unwrap().interactive);
}

#[test]
fn sync_checks_only_the_platform() {
    let h = host("g815", "x86_64-linux", "opensuse-tumbleweed");
    let probe = |platform, distro| serde_json::from_str(&probe_json(platform, distro)).unwrap();
    fspec_remote::verify_platform(&h, &probe("x86_64-linux", "nixos")).unwrap();
    let err = fspec_remote::verify_platform(&h, &probe("aarch64-linux", "nixos")).unwrap_err();
    assert_eq!(err.to_string(), "host g815 is declared as x86_64-linux but the machine is aarch64-linux");
}

#[test]
fn probe_mismatch_fails_before_shipping() {
    let (dir, runner) = fixture("");
    runner.respond(PROBE, ok(&probe_json("aarch64-linux", "fedora")));
    let h = host("g815", "x86_64-linux", "bazzite");
    let err = remote(dir.path(), &runner, false).apply(&h, "g815", None).unwrap_err();
    assert_eq!(
        err.to_string(),
        "host g815 is declared as x86_64-linux/bazzite but the machine is aarch64-linux/fedora"
    );
    assert_eq!(runner.lines().len(), 1);
}

#[test]
fn dirty_tree_is_refused_unless_allowed() {
    let (dir, runner) = fixture(" M spec.star\n");
    runner.respond(PROBE, ok(&probe_json("x86_64-linux", "bazzite")));
    let h = host("g815", "x86_64-linux", "bazzite");
    let err = remote(dir.path(), &runner, false).apply(&h, "g815", None).unwrap_err();
    assert!(err.to_string().contains("uncommitted changes") && err.to_string().contains("--dirty"), "{err}");
    assert!(!runner.lines().iter().any(|l| l.starts_with("tar") || l.starts_with("ssh -t")));

    remote(dir.path(), &runner, true).apply(&h, "g815", None).unwrap();
    let lines = runner.lines();
    assert!(lines.iter().any(|l| l.contains("-name 'abc123-dirty*' -exec rm -rf")));
}

#[test]
fn a_dirty_ship_is_named_after_its_contents_and_carries_untracked_files() {
    let (dir, runner) = with_untracked(" M spec.star\n", "new.star\n");
    fs::write(dir.path().join("spec.star"), "one").unwrap();
    fs::write(dir.path().join("new.star"), "x").unwrap();
    let r = remote(dir.path(), &runner, true);
    let first = r.ship("g815").unwrap();
    fs::write(dir.path().join("spec.star"), "two").unwrap();
    let second = r.ship("g815").unwrap();
    assert!(first.contains("abc123-dirty-") && first != second, "{first} {second}");
    let calls = runner.calls.lock().unwrap();
    let tar = calls.iter().find(|c| c.program == "tar").unwrap();
    // modules/cli.star is listed but deleted in the working tree, so it is not shipped.
    assert_eq!(tar.stdin.as_deref(), Some("spec.star\nnew.star\nfspec.lock"));
}

#[test]
fn an_older_fspec_on_the_remote_fails_before_shipping() {
    let (dir, runner) = fixture("");
    runner
        .respond(PROBE, ok(&json!({"hostname": "g815", "platform": "x86_64-linux", "distro": "bazzite"}).to_string()));
    let h = host("g815", "x86_64-linux", "bazzite");
    let err = remote(dir.path(), &runner, false).plan(&h, "g815").err().unwrap();
    assert!(err.to_string().starts_with("fspec on g815 is older than this one (engine 0 < "), "{err}");
    assert_eq!(runner.lines().len(), 1);
}

#[test]
fn missing_fspec_on_the_remote_gives_an_install_hint() {
    let (dir, runner) = fixture("");
    runner.respond(PROBE, Output { status: 127, stdout: String::new(), stderr: "fspec: not found".into() });
    let h = host("g815", "x86_64-linux", "bazzite");
    let err = remote(dir.path(), &runner, false).apply(&h, "g815", None).unwrap_err();
    assert!(err.to_string().contains("fspec is not installed on g815"), "{err}");
}

#[test]
fn plan_returns_the_remote_plan() {
    let (dir, runner) = fixture("");
    runner.respond(
        &format!("{PROBE} 'PATH=\"/usr/local/bin:$PATH\" fspec facts"),
        ok(&probe_json("x86_64-linux", "bazzite")),
    );
    runner.respond(
        &format!("{PROBE} 'PATH=\"/usr/local/bin:$PATH\" fspec --spec"),
        ok(r#"{"host":"g815","changes":[]}"#),
    );
    let h = host("g815", "x86_64-linux", "bazzite");
    let Planned::Plan(plan) = remote(dir.path(), &runner, false).plan(&h, "g815").unwrap() else {
        panic!("expected a plan")
    };
    assert_eq!(plan.host, "g815");
    assert!(runner.lines().last().unwrap().ends_with("plan --json --local --host g815'"));
}

#[test]
fn unreachable_host_is_skipped_in_plan_and_fails_apply() {
    let (dir, runner) = fixture("");
    runner.respond(PROBE, Output { status: 255, stdout: String::new(), stderr: "timed out".into() });
    let h = host("g815", "x86_64-linux", "bazzite");
    let r = remote(dir.path(), &runner, false);
    assert!(matches!(r.plan(&h, "g815").unwrap(), Planned::Unreachable));
    assert_eq!(runner.lines().len(), 1);
    let err = r.apply(&h, "g815", None).unwrap_err();
    assert!(err.to_string().contains("host g815 is unreachable over ssh (g815): timed out"), "{err}");
}

fn fleet() -> Vec<Host> {
    vec![
        host("macbook", "aarch64-darwin", ""),
        host("g815", "x86_64-linux", "bazzite"),
        host("e1504g", "x86_64-linux", "bazzite"),
    ]
}

#[test]
fn all_applies_remote_hosts_first_and_this_machine_last() {
    let hosts = fleet();
    let names: Vec<_> = order(&hosts, "macbook").iter().map(|h| h.name.as_str()).collect();
    assert_eq!(names, ["g815", "e1504g", "macbook"]);

    let mut seen = Vec::new();
    apply_all(&hosts, "macbook", |h| {
        seen.push(h.name.clone());
        Ok(())
    })
    .unwrap();
    assert_eq!(seen, ["g815", "e1504g", "macbook"]);
}

#[test]
fn all_stops_at_the_first_failure_and_summarises() {
    let hosts = fleet();
    let mut seen = Vec::new();
    let err = apply_all(&hosts, "macbook", |h| {
        seen.push(h.name.clone());
        if h.name == "e1504g" { bail!("sudo failed") } else { Ok(()) }
    })
    .unwrap_err();
    assert_eq!(seen, ["g815", "e1504g"]);
    assert_eq!(err.to_string(), "host e1504g failed: sudo failed\nconverged: g815\nnot attempted: macbook");
}

#[test]
fn ship_carries_the_cached_gh_load_files() {
    let (dir, runner) = fixture("");
    fs::write(
        dir.path().join("fspec.lock"),
        "[loads.\"gh:o/app@main\"]\nrev = \"abc\"\nfiles = { \"fspec.star\" = \"sha256-x\", \"lib.star\" = \"sha256-y\" }\n",
    )
    .unwrap();
    let cache = tempfile::tempdir().unwrap();
    fs::create_dir_all(cache.path().join("o/app/abc")).unwrap();
    fs::write(cache.path().join("o/app/abc/fspec.star"), "").unwrap();
    let r = Remote { loads_cache: Some(cache.path().to_path_buf()), ..remote(dir.path(), &runner, false) };
    let dest = r.ship("g815").unwrap();

    let calls = runner.calls.lock().unwrap();
    let tars: Vec<_> = calls.iter().filter(|c| c.program == "tar").collect();
    assert_eq!(tars.len(), 2);
    assert!(tars[1].to_string().ends_with(&format!(" -C {} -T -", cache.path().display())), "{}", tars[1]);
    assert_eq!(tars[1].stdin.as_deref(), Some("o/app/abc/fspec.star"));
    let into = format!("{dest}/.fspec/loads");
    assert_eq!(calls.last().unwrap().to_string(), format!("{PROBE} 'mkdir -p {into} && tar -xf - -C {into}'"));
}

/// An older remote, then the current one once `sudo install` ran; `oras` serves `body`.
struct Bootstrap {
    calls: std::sync::Mutex<Vec<String>>,
    body: Vec<u8>,
    installed: std::sync::atomic::AtomicBool,
}

impl fspec_core::runner::Runner for Bootstrap {
    fn run(&self, cmd: &fspec_core::runner::Cmd) -> anyhow::Result<Output> {
        use sha2::{Digest, Sha256};
        use std::sync::atomic::Ordering;
        let line = cmd.to_string();
        self.calls.lock().unwrap().push(line.clone());
        if line.contains("fspec facts --json") {
            let engine = if self.installed.load(Ordering::SeqCst) { ENGINE } else { 3 };
            return Ok(ok(
                &json!({"hostname": "g815", "platform": "x86_64-linux", "distro": "bazzite", "engine": engine})
                    .to_string(),
            ));
        }
        if line.starts_with("sh -c") && line.contains(".tar.zst") {
            fs::write(&cmd.args[3], &self.body)?;
        }
        if line.starts_with("oras pull") {
            fs::write(Path::new(cmd.args.last().unwrap()).join("fspec"), &self.body)?;
        }
        if line.starts_with("oras manifest fetch") {
            let digest: String = Sha256::digest(&self.body).iter().map(|b| format!("{b:02x}")).collect();
            let layer = json!({"digest": format!("sha256:{digest}"), "annotations": {"org.opencontainers.image.title": "fspec"}});
            return Ok(ok(&json!({"layers": [layer]}).to_string()));
        }
        if line.contains("sudo install") {
            self.installed.store(true, Ordering::SeqCst);
        }
        if line.starts_with("git ") && line.contains("ls-files") && !line.contains("--others") {
            return Ok(ok("spec.star\n"));
        }
        if line.contains("rev-parse") {
            return Ok(ok("abc123\n"));
        }
        if line.contains("plan --json") {
            return Ok(ok(r#"{"host":"g815","changes":[]}"#));
        }
        Ok(Output::default())
    }
}

/// A gzipped tar holding `bin/fspec`, the shape the engine's build recipe packs.
fn engine_tar() -> Vec<u8> {
    let mut tar = tar::Builder::new(Vec::new());
    let body = b"\x7fELF engine";
    let mut h = tar::Header::new_gnu();
    h.set_size(body.len() as u64);
    h.set_mode(0o755);
    h.set_cksum();
    tar.append_data(&mut h, "bin/fspec", &body[..]).unwrap();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut gz, &tar.into_inner().unwrap()).unwrap();
    gz.finish().unwrap()
}

#[test]
fn an_older_remote_gets_the_locked_engine_before_the_spec() {
    let dir = tempfile::tempdir().unwrap();
    let source = fspec_core::engine::pkg().source.to_string();
    let oci = "ghcr.io/o/fspec-cache/fspec@sha256:0123";
    fs::write(
        dir.path().join("fspec.lock"),
        format!(
            "[[pkg]]\nname = \"fspec\"\nsource = \"{source}\"\nrev = \"abc\"\n\n[pkg.x86_64-linux]\noci = \"{oci}\"\n"
        ),
    )
    .unwrap();
    let runner = Bootstrap { calls: Default::default(), body: engine_tar(), installed: Default::default() };
    let h = host("g815", "x86_64-linux", "bazzite");
    let r = Remote {
        runner: &runner,
        spec_dir: dir.path().into(),
        spec_file: "spec.star".into(),
        dirty: false,
        loads_cache: None,
    };
    r.plan(&h, "g815").unwrap();

    let calls = runner.calls.lock().unwrap();
    let at = |needle: &str| {
        calls.iter().position(|c| c.contains(needle)).unwrap_or_else(|| panic!("no {needle}: {calls:#?}"))
    };
    assert!(calls[at("oras pull")].contains(oci));
    assert!(calls[at("engine.new")].contains("cat >"));
    assert!(at("sudo install") < at("tar"), "{calls:#?}");
    assert!(
        calls[at("sudo install")].ends_with("/usr/local/bin/fspec && rm -f \"$HOME/.cache/fspec/engine.new\"'"),
        "{calls:#?}"
    );
}

#[test]
fn an_older_remote_with_no_locked_engine_build_says_how_to_get_one() {
    let (dir, runner) = fixture("");
    runner.respond(
        PROBE,
        ok(&json!({"hostname": "g815", "platform": "x86_64-linux", "distro": "bazzite", "engine": 3}).to_string()),
    );
    let h = host("g815", "x86_64-linux", "bazzite");
    let err = remote(dir.path(), &runner, false).plan(&h, "g815").err().unwrap();
    assert!(err.to_string().contains("fspec.lock pins no fspec; run `fspec update fspec`"), "{err}");
}

#[test]
fn an_older_remote_builds_the_locked_engine_when_ci_has_not() {
    let dir = tempfile::tempdir().unwrap();
    let source = fspec_core::engine::pkg().source.to_string();
    fs::write(
        dir.path().join("fspec.lock"),
        format!("[[pkg]]\nname = \"fspec\"\nsource = \"{source}\"\nrev = \"abc\"\n"),
    )
    .unwrap();
    let runner = Bootstrap { calls: Default::default(), body: engine_tar(), installed: Default::default() };
    let h = host("g815", "x86_64-linux", "bazzite");
    let r = Remote {
        runner: &runner,
        spec_dir: dir.path().into(),
        spec_file: "spec.star".into(),
        dirty: false,
        loads_cache: None,
    };
    r.plan(&h, "g815").unwrap();

    let calls = runner.calls.lock().unwrap();
    let at = |needle: &str| {
        calls.iter().position(|c| c.contains(needle)).unwrap_or_else(|| panic!("no {needle}: {calls:#?}"))
    };
    assert!(!calls.iter().any(|c| c.contains("oras")), "{calls:#?}");
    assert!(calls[at("job.json")].contains("cat >"));
    assert!(
        calls[at("build --job")].contains(
            "sudo env PATH=\"/usr/local/bin:$PATH\" fspec build --job job.json --platform x86_64-linux --out out &&"
        ),
        "{calls:#?}"
    );
    assert!(at("build --job") < at("fspec-x86_64-linux.tar.zst"));
    assert!(at("fspec-x86_64-linux.tar.zst") < at("rm -rf"));
    assert!(at("rm -rf") < at("engine.new"));
    assert!(at("engine.new") < at("sudo install"));
}
