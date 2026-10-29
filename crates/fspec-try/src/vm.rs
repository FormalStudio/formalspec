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

/// The guest's ssh route to the VM's host, and the placeholders `start` replaces on that host
/// with its own user and host keys.
const VM_HOST_CONFIG: &str = "/etc/ssh/ssh_config.d/50-fspec-vm-host.conf";
const VM_HOST_KEYS: &str = "/etc/fspec/vm-host.known_hosts";
const HOST_USER: &str = "@FSPEC_VM_HOST_USER@";
const HOST_KEYS: &str = "@FSPEC_VM_HOST_KEYS@";

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
    // The VM's host sits at user networking's gateway. Naming it there lets the first apply build
    // on it before the spec has placed any ssh config; `start` fills in its user and host keys.
    let on = vm.on.rsplit('@').next().unwrap_or(&vm.on);
    files.push(json!({"path": VM_HOST_CONFIG, "permissions": "0644", "content": format!(
        "Host {on}\n\tHostName 10.0.2.2\n\tHostKeyAlias {on}\n\tUser {HOST_USER}\n\tGlobalKnownHostsFile {VM_HOST_KEYS} /etc/ssh/ssh_known_hosts\n"
    )}));
    files.push(json!({"path": VM_HOST_KEYS, "permissions": "0644", "content": HOST_KEYS}));
    // The serial port is the one console qemu keeps, so the start reads the outcome there. Once it
    // has worked cloud-init is done for good: later boots, like a real host's, run without it.
    let report = format!(
        "if sh {FIRST_BOOT} >/var/log/fspec-vm-first-boot.log 2>&1; then touch /etc/cloud/cloud-init.disabled; \
         echo '{FIRST_BOOT_DONE}' >/dev/ttyS0; else tail -n 20 /var/log/fspec-vm-first-boot.log >/dev/ttyS0; echo '{FIRST_BOOT_FAILED}' >/dev/ttyS0; fi"
    );
    let data = json!({
        "hostname": g.name, "users": [user], "ssh_pwauth": false,
        "write_files": files, "runcmd": [["sh", "-c", report]],
    });
    let meta = json!({"instance-id": g.name, "local-hostname": g.name});
    (format!("#cloud-config\n{data}\n"), format!("{meta}\n"))
}

/// How qemu starts the guest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Boot {
    /// From power-on.
    Cold,
    /// Paused at power-on, for a recording to take its first frame.
    Paused,
    /// Paused in the machine `save` wrote to `$abs/ready.state`, until a `cont`.
    Saved,
}

/// The qemu command, run from inside the VM's directory. `$code`, `$disk`, `$nvram`, `$dcache`
/// and `$aio` are the script's, `$abs` too for [`Boot::Saved`]. No window: the kernel's console
/// goes to `serial.log`, the VGA display is read over `rec.sock` and typed into over
/// `qmp.sock`, the guest agent answers on `qga.sock`, ssh is forwarded to `vm.port` on the
/// loopback of the VM's host only, and qemu daemonizes once the guest is started or paused. A save
/// restores only into the same device list, so every boot takes this one.
pub fn qemu(name: &str, vm: &Vm, boot: Boot) -> Cmd {
    let cmd = Cmd::new("qemu-system-x86_64").args([
        "-name",
        &format!("fspec-vm-{name}"),
        "-machine",
        "q35,accel=kvm,pflash0=code,pflash1=vars",
        "-cpu",
        "host",
        "-smp",
        &vm.cpus.to_string(),
        "-m",
        &format!("{}G", vm.memory_gib),
        // Block devices a save writes past go in as -blockdev: qemu 11.1 cannot inactivate a
        // -drive one at the switchover, and every save fails.
        "-blockdev",
        "node-name=code,driver=file,filename=$code,read-only=on",
        "-blockdev",
        "node-name=vars,driver=file,filename=$nvram",
        "-object",
        "iothread,id=io0",
        "-blockdev",
        "node-name=disk-file,driver=file,filename=$disk,cache.direct=$direct,aio=$aio,discard=unmap",
        "-blockdev",
        "node-name=disk,driver=qcow2,file=disk-file,discard=unmap",
        "-device",
        "virtio-blk-pci,drive=disk,iothread=io0,bootindex=0",
        "-drive",
        "if=virtio,format=raw,readonly=on,file=seed.img",
        "-nic",
        &format!("user,model=virtio-net-pci,hostfwd=tcp:127.0.0.1:{}-:22", vm.port),
        "-device",
        "virtio-rng-pci",
        // A guest hands back the memory it frees, so idle VMs side by side stay small.
        "-device",
        "virtio-balloon-pci,free-page-reporting=on",
        "-device",
        "virtio-serial-pci",
        "-chardev",
        "socket,id=qga0,path=qga.sock,server=on,wait=off",
        "-device",
        "virtserialport,chardev=qga0,name=org.qemu.guest_agent.0",
        // Standard VGA (bochs-drm), not virtio-vga: a guest compositor's allocator takes any render
        // node it finds, and virtio-gpu's has no 3D behind it, so Hyprland on a guest's vkms card
        // could not draw a frame. bochs-drm has none.
        "-device",
        "VGA",
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
    match boot {
        Boot::Cold => cmd,
        Boot::Paused => cmd.arg("-S"),
        // qemu has left the VM's directory by the time it reads the state.
        Boot::Saved => cmd.args(["-S", "-incoming", "exec:zstd -dc $abs/ready.state"]),
    }
}

/// Starts qemu from inside the VM's directory, in a user scope at the CPU weight the g815's rig
/// slots run at, so a VM yields to the desktop. qemu daemonizes inside the scope and keeps it.
fn launch(name: &str, vm: &Vm, boot: Boot) -> String {
    let qemu = qemu(name, vm, boot);
    let mut s = String::from(
        "case $dcache in none) direct=on ;; *) direct=off ;; esac\n\
         cd \"$dir\"\nrm -f pid qmp.sock rec.sock qga.sock\n: > serial.log\nscope=()\n\
         if systemd-run --user --scope --quiet true >/dev/null 2>&1; then\n  \
         scope=(systemd-run --user --scope --quiet --collect -p CPUWeight=20 --unit=\"fspec-vm-$name-$$\" --)\nfi\n",
    );
    s += &format!("${{scope[@]+\"${{scope[@]}}\"}} {} \\\n", qemu.program);
    let args: Vec<String> = qemu.args.iter().map(|a| word(a)).collect();
    for pair in args.chunks(2) {
        s += &format!("  {} \\\n", pair.join(" "));
    }
    s += "  || die \"qemu did not start; is port $port of $(hostname) taken?\"\n";
    s
}

const PRELUDE: &str = r#"say() { printf 'fspec vm: %s\n' "$*" >&2; }
die() { say "$*"; exit 1; }
running() { [ -f "$dir/pid" ] && kill -0 "$(cat "$dir/pid")" 2>/dev/null; }
recording() { [ -f "$dir/rec.pid" ] && kill -0 "$(cat "$dir/rec.pid")" 2>/dev/null; }
qmp() {
  command -v socat >/dev/null || die "socat is not installed on $(hostname)"
  { printf '%s\n' '{"execute":"qmp_capabilities"}'; cat; } | socat -t 0.5 - "UNIX-CONNECT:$dir/qmp.sock"
}
# The last "status" a QMP reply on stdin carries.
qmp_status() { sed -n 's/.*"status": *"\([a-z-]*\)".*/\1/p' | tail -n 1; }
# One command to the guest agent and its one-line answer. The agent writes an answer even once the
# asker has gone, and the next connection reads it, so a guest-sync goes first.
qga() {
  command -v socat >/dev/null || die "socat is not installed on $(hostname)"
  local id=$RANDOM$RANDOM line=
  coproc QGA { socat - "UNIX-CONNECT:$dir/qga.sock" 2>/dev/null; }
  printf '{"execute":"guest-sync","arguments":{"id":%s}}\n' "$id" >&"${QGA[1]}"
  while read -r -t 10 line <&"${QGA[0]}"; do [ "$line" = "{\"return\": $id}" ] && break; done
  [ "$line" = "{\"return\": $id}" ] && printf '%s\n' "$1" >&"${QGA[1]}" && read -r -t 10 line <&"${QGA[0]}" || line=
  kill "$QGA_PID" 2>/dev/null || true
  wait "$QGA_PID" 2>/dev/null || true
  printf '%s\n' "$line"
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
    let on = q(vm.on.rsplit('@').next().unwrap_or(&vm.on));
    // Each key line ends in a JSON `\n`; a quoted replacement keeps bash from reading `&` or `\`.
    s += &format!(
        "  keys=$(for k in /etc/ssh/ssh_host_*_key.pub; do printf '%s %s\\\\n' {on} \"$(cut -d' ' -f1,2 \"$k\")\"; done)\n  \
         user_data={}\n  user_data=${{user_data//{HOST_USER}/\"$(id -un)\"}}\n  user_data=${{user_data//{HOST_KEYS}/\"$keys\"}}\n  \
         printf %s \"$user_data\" > \"$dir/user-data\"\n  printf %s {} > \"$dir/meta-data\"\n",
        q(&user_data),
        q(&meta_data)
    );
    s += "  qemu-img create -q -f raw \"$dir/seed.img.part\" 2M\n  mkfs.vfat -n CIDATA \"$dir/seed.img.part\" >/dev/null\n";
    s += "  mcopy -i \"$dir/seed.img.part\" \"$dir/user-data\" \"$dir/meta-data\" ::\n";
    s += "  mv \"$dir/seed.img.part\" \"$dir/seed.img\"\n  mv \"$dir/disk.qcow2.part\" \"$dir/disk.qcow2\"\n";
    s += "  new=1\n  say \"created $dir/disk.qcow2 from $(basename \"$image\")\"\nfi\n";
    // The disk is about to move on from the machine a save holds.
    s += "if [ -f \"$dir/ready.state\" ]; then rm -f \"$dir/ready.state\"; say \"dropped $name's saved state; fspec vm save takes a new one\"; fi\n";
    s +=
        "if [ -f \"$dir/run\" ]; then rm -rf \"$(cat \"$dir/run\")\" \"$dir/run\"; fi\ndisk=disk.qcow2 nvram=vars.fd\n";
    s += &crate::disk_probe("$dir/disk.qcow2", true);
    s += &launch(&g.name, vm, if record.is_some() { Boot::Paused } else { Boot::Cold });
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

/// Saves the running VM's whole machine (RAM and device state) to `ready.state` once its boot has
/// settled, and stops it. The disk stays the one that machine was running on: a cold start drops
/// the save, and [`resume`] runs each time on a throwaway overlay of it.
pub fn save(name: &str) -> Result<String> {
    if !crate::valid_name(name) {
        bail!("`{name}` is not a VM name");
    }
    let mut s = format!("#!/usr/bin/env bash\nset -eu\numask 077\nname={}\ndir={}\n", q(name), dir(name));
    s += PRELUDE;
    s += r#"running || die "$name is not running; fspec vm start boots it"
[ ! -f "$dir/run" ] || die "$name runs from its saved state; fspec vm stop it, then start and save it"
recording && die "$name is being recorded; fspec vm frames waits for it"
command -v zstd >/dev/null || die "zstd is not installed on $(hostname)"
abs=$(cd "$dir" && pwd)
pid=$(cat "$dir/pid")
# A settled guest with an empty page cache resumes into the same moment every time, from a
# smaller state.
say "waiting for $name to finish booting"
settle='systemctl is-system-running --wait >/dev/null; sync; echo 3 >/proc/sys/vm/drop_caches'
r=$(qga "{\"execute\":\"guest-exec\",\"arguments\":{\"path\":\"/bin/sh\",\"arg\":[\"-c\",\"$settle\"]}}")
[ -n "$r" ] || die "$name's guest agent did not answer on qga.sock"
gpid=$(printf %s "$r" | sed -n 's/.*"pid": *\([0-9]*\).*/\1/p')
[ -n "$gpid" ] || die "$name's guest agent did not run the settle: $r"
for _ in $(seq 300); do
  qga "{\"execute\":\"guest-exec-status\",\"arguments\":{\"pid\":$gpid}}" | grep -q '"exited": *true' && break
  sleep 1
done
rm -f "$abs/ready.state.part"
echo '{"execute":"stop"}' | qmp >/dev/null
printf '{"execute":"migrate","arguments":{"uri":"exec:zstd -1 -T0 > %s/ready.state.part"}}\n' "$abs" | qmp >/dev/null
st=
for _ in $(seq 1200); do
  st=$(echo '{"execute":"query-migrate"}' | qmp | qmp_status)
  case $st in completed) break ;; failed | cancelled) die "saving $name failed ($st)" ;; esac
  sleep 0.5
done
[ "$st" = completed ] || die "saving $name did not finish"
echo '{"execute":"quit"}' | qmp >/dev/null 2>&1 || true
while kill -0 "$pid" 2>/dev/null; do sleep 0.2; done
rm -f "$dir/pid"
mv "$abs/ready.state.part" "$abs/ready.state"
say "saved $name ($(du -h "$abs/ready.state" | cut -f1)); fspec vm resume starts it from there"
"#;
    Ok(s)
}

/// Starts the VM from its saved machine on a new overlay of its disk and a copy of its UEFI
/// variables, both in `$TMPDIR` (tmpfs on most hosts), so nothing a run does reaches the next one.
/// Sets the guest's clock, which is the save's, and returns once its sshd answers.
pub fn resume(name: &str, vm: &Vm) -> Result<String> {
    if !crate::valid_name(name) {
        bail!("`{name}` is not a VM name");
    }
    let mut s =
        format!("#!/usr/bin/env bash\nset -eu\numask 077\nname={}\nport={}\ndir={}\n", q(name), vm.port, dir(name));
    s += PRELUDE;
    s += r#"if running; then say "$name is already running, ssh on 127.0.0.1:$port of $(hostname)"; exit 0; fi
[ -f "$dir/ready.state" ] || die "$name has no saved state; fspec vm start it, then fspec vm save it"
command -v zstd >/dev/null || die "zstd is not installed on $(hostname)"
t0=$(date +%s%N)
abs=$(cd "$dir" && pwd)
run="${TMPDIR:-/tmp}/fspec-vm-$(id -u)-$name"
rm -rf "$run"
mkdir -p "$run"
qemu-img create -q -f qcow2 -b "$abs/disk.qcow2" -F qcow2 "$run/disk.qcow2"
cp "$abs/vars.fd" "$run/vars.fd"
printf '%s\n' "$run" >"$dir/run"
disk=$run/disk.qcow2 nvram=$run/vars.fd
"#;
    s += &crate::disk_probe("$run/disk.qcow2", true);
    s += "code=\n";
    for (c, v) in OVMF {
        s += &format!("[ -n \"$code\" ] || {{ [ -f {c} ] && [ -f {v} ] && code={c}; }} || true\n");
    }
    s += "[ -n \"$code\" ] || die \"no OVMF firmware on $(hostname); install ovmf or edk2-ovmf\"\n";
    s += &launch(name, vm, Boot::Saved);
    s += &format!(
        r#"st=
for _ in $(seq 1200); do
  st=$(echo '{{"execute":"query-status"}}' | qmp | qmp_status)
  [ "$st" = paused ] && break
  running || die "$name stopped while its saved state loaded"
  sleep 0.1
done
[ "$st" = paused ] || die "$name's saved state did not load"
echo '{{"execute":"cont"}}' | qmp >/dev/null
# The agent sets the system clock, then the RTC through hwclock, which SELinux keeps from
# /etc/adjtime on Tumbleweed; that second step failing leaves the clock set.
qga "{{\"execute\":\"guest-set-time\",\"arguments\":{{\"time\":$(date +%s%N)}}}}" | grep -qE '"return"|hardware clock' ||
  say "$name's guest agent did not set its clock; it reads the time of the save"
for _ in $(seq {SSH_WAIT}); do
  banner=$(timeout 3 bash -c "exec 3<>/dev/tcp/127.0.0.1/$port && head -c 4 <&3" 2>/dev/null || true)
  [ "$banner" = SSH- ] && break
  running || die "$name stopped after it resumed"
  sleep 0.1
done
[ "$banner" = SSH- ] || die "$name resumed but its sshd did not answer"
ms=$((($(date +%s%N) - t0) / 1000000))
say "$name resumed from its saved state in $((ms / 1000)).$(printf %03d $((ms % 1000))) s, ssh on 127.0.0.1:$port of $(hostname)"
"#
    );
    Ok(s)
}

/// Powers the VM off through ACPI, as its power button would, and stops qemu after
/// [`POWEROFF_WAIT`] seconds if the guest has not. A VM resumed from its save is stopped outright
/// and its overlay deleted: nothing on it is kept.
pub fn stop(name: &str) -> Result<String> {
    if !crate::valid_name(name) {
        bail!("`{name}` is not a VM name");
    }
    let mut s = format!("set -eu\nname={}\ndir={}\n", q(name), dir(name));
    s += PRELUDE;
    s += &format!(
        r#"running || {{ say "$name is not running"; exit 0; }}
pid=$(cat "$dir/pid")
if [ -f "$dir/run" ]; then
  echo '{{"execute":"quit"}}' | qmp >/dev/null 2>&1 || kill "$pid"
  while kill -0 "$pid" 2>/dev/null; do sleep 0.2; done
  rm -rf "$(cat "$dir/run")" "$dir/run" "$dir/pid"
  say "$name stopped; its run is gone, its saved state stays"
  exit 0
fi
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

/// One line per VM: `<name> running|resumed|saved|stopped|absent <port>`.
pub fn status(vms: &[(&str, u16)]) -> Result<String> {
    let mut s = String::from("set -eu\n");
    s += PRELUDE;
    for (name, port) in vms {
        if !crate::valid_name(name) {
            bail!("`{name}` is not a VM name");
        }
        s += &format!(
            "dir={}\nif running; then state=running; [ -f \"$dir/run\" ] && state=resumed; \
             elif [ -f \"$dir/ready.state\" ]; then state=saved; elif [ -f \"$dir/disk.qcow2\" ]; then state=stopped; \
             else state=absent; fi\nprintf '%s\\t%s\\t%s\\n' {} \"$state\" {port}\n",
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
