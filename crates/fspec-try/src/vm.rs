//! `fspec vm`: a host declared with `vm(...)` runs as a KVM guest on another machine, with no
//! window: its display is read back as PNG frames over QMP and typed into the same way. Its disk
//! starts as the distro's cloud image, which cloud-init gives the host's first user, its keys,
//! its name and the distro's first-boot setup; `fspec apply` installs the rest like on any host.
//! As with `fspec try`, each step is one bash script for the VM's host, which needs qemu, OVMF,
//! curl, socat and (for the first boot) dosfstools and mtools, not this fspec.

use anyhow::{Result, bail};
use fspec_core::resource::Vm;
use fspec_core::runner::Cmd;
use serde_json::json;

use crate::{Iso, OVMF, q, word};

/// The cloud image of each distro `fspec vm` boots without a `vm(image = ...)`, with the
/// checksum file its project publishes beside it.
const IMAGES: [(&str, &str); 1] = [(
    "opensuse-tumbleweed",
    "https://download.opensuse.org/tumbleweed/appliances/openSUSE-Tumbleweed-Minimal-VM.x86_64-Cloud.qcow2",
)];

/// The image a new disk starts from: `vm.image`, else the distro's own.
pub fn image_url(distro: &str, vm: &Vm) -> Result<String> {
    if let Some(url) = &vm.image {
        return Ok(url.clone());
    }
    match IMAGES.iter().find(|(d, _)| *d == distro) {
        Some((_, url)) => Ok(url.to_string()),
        None => bail!("fspec vm has no cloud image for {distro}; set vm(image = ...) to a qcow2 with cloud-init"),
    }
}

/// The image's SHA-256 from the `<url>.sha256` list its project publishes, if it lists one.
pub fn image_sum(list: &str, iso: &Iso) -> Option<String> {
    crate::sum(list, &iso.file)
}

/// Who the guest is on its first boot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Guest {
    /// The host name, which also names the VM's directory on the VM's host.
    pub name: String,
    pub user: String,
    pub uid: Option<u32>,
    /// Files written on the first boot, `(path, content, executable)`, before `first_boot` runs.
    pub files: Vec<(String, String, bool)>,
    /// A script run as root once on the first boot; the start waits for it.
    pub first_boot: Option<String>,
}

/// Where the first-boot script lands in the guest, and the lines it reports on the serial port,
/// which `start` waits for on a new disk.
const FIRST_BOOT: &str = "/var/lib/fspec/vm-first-boot.sh";
pub const FIRST_BOOT_DONE: &str = "fspec vm: first boot done";
pub const FIRST_BOOT_FAILED: &str = "fspec vm: first boot failed";

/// Where a VM lives on the VM's host, as a bash word.
fn dir(name: &str) -> String {
    format!("\"${{XDG_DATA_HOME:-$HOME/.local/share}}/fspec/vm\"/{}", q(name))
}

/// cloud-init's NoCloud `user-data` and `meta-data`. JSON is YAML, so serde does the quoting. The
/// user can sudo without a password: the first `fspec apply` needs it before the spec has
/// declared anything.
pub fn seed(g: &Guest, vm: &Vm) -> (String, String) {
    let mut user = json!({
        "name": g.user,
        "sudo": "ALL=(ALL) NOPASSWD:ALL",
        "shell": "/bin/bash",
        "lock_passwd": true,
        "ssh_authorized_keys": vm.authorized_keys,
    });
    if let Some(uid) = g.uid {
        user["uid"] = json!(uid);
    }
    let mut files: Vec<serde_json::Value> = g
        .files
        .iter()
        .map(|(path, content, exec)| {
            json!({"path": path, "content": content, "permissions": if *exec { "0755" } else { "0644" }})
        })
        .collect();
    files.push(json!({"path": FIRST_BOOT, "content": g.first_boot.as_deref().unwrap_or(""), "permissions": "0700"}));
    // The serial port is the one console qemu keeps, so the start reads the outcome there.
    let report = format!(
        "if sh {FIRST_BOOT} >/var/log/fspec-vm-first-boot.log 2>&1; then echo '{FIRST_BOOT_DONE}' >/dev/ttyS0; \
         else tail -n 20 /var/log/fspec-vm-first-boot.log >/dev/ttyS0; echo '{FIRST_BOOT_FAILED}' >/dev/ttyS0; fi"
    );
    let data = json!({
        "hostname": g.name, "users": [user], "ssh_pwauth": false,
        "write_files": files, "runcmd": [["sh", "-c", report]],
    });
    let meta = json!({"instance-id": g.name, "local-hostname": g.name});
    (format!("#cloud-config\n{data}\n"), format!("{meta}\n"))
}

/// The qemu command, run from inside the VM's directory. `$code`, `$dcache` and `$aio` are the
/// script's. No window: the kernel's console goes to `serial.log`, the virtio-vga display is read
/// over `rec.sock` and typed into over `qmp.sock`, ssh is forwarded to `vm.port` on the loopback
/// of the VM's host only, and qemu daemonizes once the guest is started, or paused at power-on
/// when `paused`, for a recording to start first.
pub fn qemu(name: &str, vm: &Vm, paused: bool) -> Cmd {
    let cmd = Cmd::new("qemu-system-x86_64").args([
        "-name",
        &format!("fspec-vm-{name}"),
        "-machine",
        "q35,accel=kvm",
        "-cpu",
        "host",
        "-smp",
        &vm.cpus.to_string(),
        "-m",
        &format!("{}G", vm.memory_gib),
        "-drive",
        "if=pflash,format=raw,readonly=on,file=$code",
        "-drive",
        "if=pflash,format=raw,file=vars.fd",
        "-object",
        "iothread,id=io0",
        "-drive",
        "if=none,id=disk,format=qcow2,discard=unmap,cache=$dcache,aio=$aio,file=disk.qcow2",
        "-device",
        "virtio-blk-pci,drive=disk,iothread=io0,bootindex=0",
        "-drive",
        "if=virtio,format=raw,readonly=on,file=seed.img",
        "-nic",
        &format!("user,model=virtio-net-pci,hostfwd=tcp:127.0.0.1:{}-:22", vm.port),
        "-device",
        "virtio-rng-pci",
        "-device",
        "virtio-vga",
        "-display",
        "none",
        "-serial",
        "file:serial.log",
        "-qmp",
        "unix:qmp.sock,server=on,wait=off",
        "-qmp",
        "unix:rec.sock,server=on,wait=off",
        "-pidfile",
        "pid",
        "-daemonize",
    ]);
    if paused { cmd.arg("-S") } else { cmd }
}

const PRELUDE: &str = r#"say() { printf 'fspec vm: %s\n' "$*" >&2; }
die() { say "$*"; exit 1; }
running() { [ -f "$dir/pid" ] && kill -0 "$(cat "$dir/pid")" 2>/dev/null; }
recording() { [ -f "$dir/rec.pid" ] && kill -0 "$(cat "$dir/rec.pid")" 2>/dev/null; }
qmp() {
  command -v socat >/dev/null || die "socat is not installed on $(hostname)"
  { printf '%s\n' '{"execute":"qmp_capabilities"}'; cat; } | socat -t 2 - "UNIX-CONNECT:$dir/qmp.sock"
}
"#;

/// Seconds `start` waits for the guest's sshd and for a new disk's first boot, and `stop` for
/// the guest to power off.
pub const SSH_WAIT: u32 = 300;
pub const FIRST_BOOT_WAIT: u32 = 900;
pub const POWEROFF_WAIT: u32 = 120;

/// A recording of the VM's display: one PNG every `every_ms`, frame `n` taken `n * every_ms`
/// after the recording started, for `seconds` or until qemu exits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record {
    pub every_ms: u32,
    pub seconds: u32,
}

/// Runs on the VM's host beside qemu, detached: `$1` is the VM's directory, absolute, since qemu
/// writes the frames relative to `/` once it has daemonized. Commands go out on the clock without
/// waiting for replies, so a slow frame delays the ones after it rather than the schedule.
const RECORDER: &str = r#"#!/usr/bin/env bash
set -u
dir=$1 every=$2 seconds=$3
ns() { date +%s%N; }
{
  printf '%s\n' '{"execute":"qmp_capabilities"}'
  start=$(ns)
  end=$((start + seconds * 1000000000))
  n=0
  while [ "$(ns)" -lt "$end" ]; do
    printf '{"execute":"screendump","arguments":{"filename":"%s/frames/%06d.png","format":"png"}}\n' "$dir" "$n"
    n=$((n + 1))
    wait=$((start + n * every * 1000000 - $(ns)))
    [ "$wait" -le 0 ] || sleep "$((wait / 1000000000)).$(printf %09d $((wait % 1000000000)))"
  done
} | socat -t 5 - "UNIX-CONNECT:$dir/rec.sock" >"$dir/rec.log" 2>&1
"#;

/// Starts [`RECORDER`] into a fresh `frames/`, and returns once its first frame is on disk.
fn record_start(r: &Record) -> String {
    format!(
        r#"recording && die "$name is being recorded already; fspec vm frames waits for it"
abs=$(cd "$dir" && pwd)
rm -rf "$abs/frames"
mkdir -p "$abs/frames"
printf %s {recorder} >"$abs/record.sh"
setsid bash "$abs/record.sh" "$abs" {every} {seconds} </dev/null >/dev/null 2>&1 &
echo $! >"$abs/rec.pid"
for _ in $(seq 100); do [ -e "$abs/frames/000000.png" ] && break; sleep 0.05; done
[ -e "$abs/frames/000000.png" ] || {{ cat "$abs/rec.log" >&2 2>/dev/null; die "the recorder took no frame"; }}
say "recording $name, a frame every {every} ms for {seconds} s, into $abs/frames"
"#,
        recorder = q(RECORDER),
        every = r.every_ms,
        seconds = r.seconds,
    )
}

/// Boots the VM unless it runs already, creating its disk from `image` on the first start, and
/// returns once the guest's sshd answers on the forwarded port and a new disk's first boot is
/// done. With `record`, the VM must be off: qemu starts paused, the recorder takes its first
/// frame, and only then does the firmware run, so the frames begin at power-on.
pub fn start(g: &Guest, vm: &Vm, image: &Iso, record: Option<&Record>) -> Result<String> {
    if !crate::valid_name(&g.name) {
        bail!("`{}` is not a VM name", g.name);
    }
    let mut s = String::from("#!/usr/bin/env bash\nset -eu\numask 077\n");
    s += &format!("name={}\nport={}\ndir={}\n", q(&g.name), vm.port, dir(&g.name));
    s += "images=\"${XDG_CACHE_HOME:-$HOME/.cache}/fspec/vm/images\"\n";
    s += PRELUDE;
    if record.is_some() {
        s += "running && die \"$name is running; fspec vm stop it to record from power-on\"\n";
    } else {
        s += "if running; then say \"$name is already running, ssh on 127.0.0.1:$port of $(hostname)\"; exit 0; fi\n";
    }
    s += "command -v qemu-system-x86_64 >/dev/null || die \"qemu-system-x86_64 is not installed on $(hostname)\"\n";
    s += "[ -w /dev/kvm ] || die \"/dev/kvm is missing or not writable for $(id -un) on $(hostname)\"\n";
    s += "code=\nvars=\n";
    for (c, v) in OVMF {
        s += &format!("[ -n \"$code\" ] || {{ [ -f {c} ] && [ -f {v} ] && code={c} vars={v}; }} || true\n");
    }
    s += "[ -n \"$code\" ] || die \"no OVMF firmware on $(hostname); install ovmf or edk2-ovmf\"\n";
    s += "mkdir -p \"$dir\" \"$images\"\nnew=\n";
    let file = q(&image.file);
    let url = q(&image.url);
    let check = match &image.sha256 {
        Some(sha) => format!("echo \"{sha}  $image\" | sha256sum -c --status -"),
        None => "false".into(),
    };
    s += &format!("if [ ! -f \"$dir/disk.qcow2\" ]; then\n  image=\"$images\"/{file}\n");
    if image.sha256.is_none() {
        s += &format!("  [ -f \"$image\" ] || die \"no published checksum for \"{url}\"; cannot verify a download\"\n");
    }
    s += &format!(
        "  if [ ! -f \"$image\" ] || ! {check}; then\n    say \"downloading {}\"\n    rm -f \"$image\"\n    curl -fL --retry 3 --progress-bar -o \"$image.part\" {url}\n",
        image.file
    );
    if let Some(sha) = &image.sha256 {
        s += &format!(
            "    echo \"{sha}  $image.part\" | sha256sum -c --status - || {{ rm -f \"$image.part\"; die \"{} does not match its published checksum; run again\"; }}\n",
            image.file
        );
    }
    s += "    mv \"$image.part\" \"$image\"\n  fi\n";
    let (user_data, meta_data) = seed(g, vm);
    s += "  for c in mkfs.vfat mcopy; do command -v \"$c\" >/dev/null || die \"$c is not installed on $(hostname); install dosfstools and mtools\"; done\n";
    s += "  rm -f \"$dir\"/*.part\n";
    s += &format!(
        "  qemu-img convert -O qcow2 -o nocow=on \"$image\" \"$dir/disk.qcow2.part\"\n  qemu-img resize -q \"$dir/disk.qcow2.part\" {}G\n",
        vm.disk_gib
    );
    s += "  cp \"$vars\" \"$dir/vars.fd\"\n";
    s += &format!(
        "  printf %s {} > \"$dir/user-data\"\n  printf %s {} > \"$dir/meta-data\"\n",
        q(&user_data),
        q(&meta_data)
    );
    s += "  qemu-img create -q -f raw \"$dir/seed.img.part\" 2M\n  mkfs.vfat -n CIDATA \"$dir/seed.img.part\" >/dev/null\n";
    s += "  mcopy -i \"$dir/seed.img.part\" \"$dir/user-data\" \"$dir/meta-data\" ::\n";
    s += "  mv \"$dir/seed.img.part\" \"$dir/seed.img\"\n  mv \"$dir/disk.qcow2.part\" \"$dir/disk.qcow2\"\n";
    s += "  new=1\n  say \"created $dir/disk.qcow2 from $(basename \"$image\")\"\nfi\n";
    s += &crate::disk_probe("$dir/disk.qcow2", true);
    let qemu = qemu(&g.name, vm, record.is_some());
    s += "cd \"$dir\"\nrm -f pid qmp.sock rec.sock\n: > serial.log\n";
    s += &format!("{} \\\n", qemu.program);
    let args: Vec<String> = qemu.args.iter().map(|a| word(a)).collect();
    for pair in args.chunks(2) {
        s += &format!("  {} \\\n", pair.join(" "));
    }
    s += "  || die \"qemu did not start; is port $port of $(hostname) taken?\"\n";
    if let Some(r) = record {
        s += &record_start(r);
        s += "echo '{\"execute\":\"cont\"}' | qmp >/dev/null\n";
    }
    s += &format!(
        r#"for _ in $(seq {SSH_WAIT}); do
  running || {{ tail -n 20 serial.log >&2; die "$name stopped while booting"; }}
  banner=$(timeout 3 bash -c "exec 3<>/dev/tcp/127.0.0.1/$port && head -c 4 <&3" 2>/dev/null || true)
  [ "$banner" = SSH- ] && break
  sleep 1
done
[ "$banner" = SSH- ] || {{ tail -n 20 serial.log >&2; die "$name runs but its sshd did not answer in {SSH_WAIT} s"; }}
if [ -n "$new" ]; then
  say "waiting for the first boot's setup"
  for _ in $(seq {FIRST_BOOT_WAIT}); do
    grep -qF '{FIRST_BOOT_DONE}' serial.log && break
    grep -qF '{FIRST_BOOT_FAILED}' serial.log && {{ tail -n 25 serial.log >&2; die "$name's first boot failed"; }}
    running || die "$name stopped during its first boot"
    sleep 1
  done
  grep -qF '{FIRST_BOOT_DONE}' serial.log || die "$name's first boot did not finish in {FIRST_BOOT_WAIT} s"
fi
say "$name is up ({} CPUs, {} GiB), ssh on 127.0.0.1:$port of $(hostname)"
"#,
        vm.cpus, vm.memory_gib
    );
    Ok(s)
}

/// Powers the VM off through ACPI, as its power button would, and stops qemu after
/// [`POWEROFF_WAIT`] seconds if the guest has not.
pub fn stop(name: &str) -> Result<String> {
    if !crate::valid_name(name) {
        bail!("`{name}` is not a VM name");
    }
    let mut s = format!("set -eu\nname={}\ndir={}\n", q(name), dir(name));
    s += PRELUDE;
    s += &format!(
        r#"running || {{ say "$name is not running"; exit 0; }}
pid=$(cat "$dir/pid")
if command -v socat >/dev/null; then
  echo '{{"execute":"system_powerdown"}}' | qmp >/dev/null 2>&1 || true
else
  say "no socat on $(hostname) to ask the guest to power off"
fi
for _ in $(seq {POWEROFF_WAIT}); do kill -0 "$pid" 2>/dev/null || break; sleep 1; done
if kill -0 "$pid" 2>/dev/null; then
  say "$name did not power off, stopping qemu"
  kill "$pid"
  while kill -0 "$pid" 2>/dev/null; do sleep 1; done
fi
rm -f "$dir/pid"
say "$name stopped"
"#
    );
    Ok(s)
}

/// One line per VM: `<name> running|stopped|absent <port>`.
pub fn status(vms: &[(&str, u16)]) -> Result<String> {
    let mut s = String::from("set -eu\n");
    s += PRELUDE;
    for (name, port) in vms {
        if !crate::valid_name(name) {
            bail!("`{name}` is not a VM name");
        }
        s += &format!(
            "dir={}\nif running; then state=running; elif [ -f \"$dir/disk.qcow2\" ]; then state=stopped; else state=absent; fi\nprintf '%s\\t%s\\t%s\\n' {} \"$state\" {port}\n",
            dir(name),
            q(name)
        );
    }
    Ok(s)
}

/// Records the display of a running VM, as `start` does from power-on.
pub fn record(name: &str, r: &Record) -> Result<String> {
    if !crate::valid_name(name) {
        bail!("`{name}` is not a VM name");
    }
    if r.every_ms == 0 || r.seconds == 0 {
        bail!("a recording needs a frame interval and a length above zero");
    }
    let mut s = format!("set -eu\nname={}\ndir={}\n", q(name), dir(name));
    s += PRELUDE;
    s += "running || die \"$name is not running; fspec vm record --boot powers it on\"\n";
    s += &record_start(r);
    Ok(s)
}

/// Waits for the VM's recording to end, then writes its frames to stdout as a tar of `frames/`.
pub fn frames(name: &str) -> Result<String> {
    if !crate::valid_name(name) {
        bail!("`{name}` is not a VM name");
    }
    let mut s = format!("set -eu\nname={}\ndir={}\n", q(name), dir(name));
    s += PRELUDE;
    s += r#"[ -d "$dir/frames" ] || die "$name has no recording"
if recording; then
  say "waiting for the recording of $name to end"
  while recording; do sleep 1; done
fi
tar -C "$dir" -cf - frames
"#;
    Ok(s)
}

/// One key press as QMP `send-key` qcodes, the modifiers first. Typed through a US layout, so the
/// guest's keymap has to be `us` for text to arrive as written.
pub fn text_keys(text: &str) -> Result<Vec<Vec<String>>> {
    // The keys NAMES names, unshifted and shifted, at the same index; all ASCII, so a byte index
    // is a key's index.
    const PLAIN: &str = "`1234567890-=[]\\;',./";
    const SHIFTED: &str = "~!@#$%^&*()_+{}|:\"<>?";
    const NAMES: [&str; 21] = [
        "grave_accent",
        "1",
        "2",
        "3",
        "4",
        "5",
        "6",
        "7",
        "8",
        "9",
        "0",
        "minus",
        "equal",
        "bracket_left",
        "bracket_right",
        "backslash",
        "semicolon",
        "apostrophe",
        "comma",
        "dot",
        "slash",
    ];
    text.chars()
        .map(|c| {
            let key = |k: &str| vec![k.to_string()];
            let shift = |k: &str| vec!["shift".to_string(), k.to_string()];
            Ok(match c {
                'a'..='z' => key(&c.to_string()),
                'A'..='Z' => shift(&c.to_ascii_lowercase().to_string()),
                ' ' => key("spc"),
                '\n' => key("ret"),
                '\t' => key("tab"),
                _ => match (PLAIN.find(c), SHIFTED.find(c)) {
                    (Some(i), _) => key(NAMES[i]),
                    (_, Some(i)) => shift(NAMES[i]),
                    _ => bail!("{c:?} has no key on a US keyboard; send it with fspec vm key"),
                },
            })
        })
        .collect()
}

/// A combination like `ctrl-alt-f2` or `ret` as qcodes, the way QEMU's own `sendkey` spells them.
pub fn combo_keys(combo: &str) -> Result<Vec<String>> {
    let keys: Vec<String> = combo.split('-').map(str::to_string).collect();
    if keys.iter().any(|k| k.is_empty() || !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')) {
        bail!("`{combo}` is not a key combination like ret, shift-tab or ctrl-alt-f2");
    }
    Ok(keys)
}

/// Presses each key combination in turn on the VM's keyboard.
pub fn send_keys(name: &str, presses: &[Vec<String>]) -> Result<String> {
    if !crate::valid_name(name) {
        bail!("`{name}` is not a VM name");
    }
    let mut s = format!("set -eu\nname={}\ndir={}\n", q(name), dir(name));
    s += PRELUDE;
    s += "running || die \"$name is not running\"\n";
    // QEMU queues the presses and releases, but a guest reading a burst can still drop repeats of
    // one key, so each press gets its own moment.
    s += "out=$({\n";
    for keys in presses {
        let keys: Vec<_> = keys.iter().map(|k| json!({"type": "qcode", "data": k})).collect();
        let line = json!({"execute": "send-key", "arguments": {"keys": keys, "hold-time": 30}}).to_string();
        s += &format!("  echo {}\n  sleep 0.06\n", q(&line));
    }
    s += "} | qmp)\n";
    s += "case $out in *'\"error\"'*) printf '%s\\n' \"$out\" | grep '\"error\"' >&2; die \"$name refused a key\" ;; esac\n";
    Ok(s)
}
