//! `fspec vm`: a host declared with `vm(...)` runs as a headless KVM guest on another machine. Its
//! disk starts as the distro's cloud image, which cloud-init gives the host's first user, its
//! keys and its name on the first boot; `fspec apply` installs the rest like on any host. As with
//! `fspec try`, each step is one bash script for the VM's host, which needs qemu, OVMF, curl and
//! (for the first boot) dosfstools and mtools, not this fspec.

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
}

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
    let data = json!({"hostname": g.name, "users": [user], "ssh_pwauth": false});
    let meta = json!({"instance-id": g.name, "local-hostname": g.name});
    (format!("#cloud-config\n{data}\n"), format!("{meta}\n"))
}

/// The qemu command, run from inside the VM's directory. `$code`, `$dcache` and `$aio` are the
/// script's. Headless: the console goes to `serial.log`, ssh is forwarded to `vm.port` on the
/// loopback of the VM's host only, and qemu daemonizes once the guest is started.
pub fn qemu(name: &str, vm: &Vm) -> Cmd {
    Cmd::new("qemu-system-x86_64").args([
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
        "-display",
        "none",
        "-serial",
        "file:serial.log",
        "-qmp",
        "unix:qmp.sock,server=on,wait=off",
        "-pidfile",
        "pid",
        "-daemonize",
    ])
}

const PRELUDE: &str = r#"say() { printf 'fspec vm: %s\n' "$*" >&2; }
die() { say "$*"; exit 1; }
running() { [ -f "$dir/pid" ] && kill -0 "$(cat "$dir/pid")" 2>/dev/null; }
"#;

/// Seconds `start` waits for the guest's sshd, and `stop` for the guest to power off.
pub const SSH_WAIT: u32 = 300;
pub const POWEROFF_WAIT: u32 = 120;

/// Boots the VM unless it runs already, creating its disk from `image` on the first start, and
/// returns once the guest's sshd answers on the forwarded port.
pub fn start(g: &Guest, vm: &Vm, image: &Iso) -> Result<String> {
    if !crate::valid_name(&g.name) {
        bail!("`{}` is not a VM name", g.name);
    }
    let mut s = String::from("#!/usr/bin/env bash\nset -eu\numask 077\n");
    s += &format!("name={}\nport={}\ndir={}\n", q(&g.name), vm.port, dir(&g.name));
    s += "images=\"${XDG_CACHE_HOME:-$HOME/.cache}/fspec/vm/images\"\n";
    s += PRELUDE;
    s += "if running; then say \"$name is already running, ssh on 127.0.0.1:$port of $(hostname)\"; exit 0; fi\n";
    s += "command -v qemu-system-x86_64 >/dev/null || die \"qemu-system-x86_64 is not installed on $(hostname)\"\n";
    s += "[ -w /dev/kvm ] || die \"/dev/kvm is missing or not writable for $(id -un) on $(hostname)\"\n";
    s += "code=\nvars=\n";
    for (c, v) in OVMF {
        s += &format!("[ -n \"$code\" ] || {{ [ -f {c} ] && [ -f {v} ] && code={c} vars={v}; }} || true\n");
    }
    s += "[ -n \"$code\" ] || die \"no OVMF firmware on $(hostname); install ovmf or edk2-ovmf\"\n";
    s += "mkdir -p \"$dir\" \"$images\"\n";
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
    s += "  say \"created $dir/disk.qcow2 from $(basename \"$image\")\"\nfi\n";
    s += &crate::disk_probe("$dir/disk.qcow2", true);
    let qemu = qemu(&g.name, vm);
    s += "cd \"$dir\"\nrm -f pid qmp.sock\n: > serial.log\n";
    s += &format!("{} \\\n", qemu.program);
    let args: Vec<String> = qemu.args.iter().map(|a| word(a)).collect();
    for pair in args.chunks(2) {
        s += &format!("  {} \\\n", pair.join(" "));
    }
    s += "  || die \"qemu did not start; is port $port of $(hostname) taken?\"\n";
    s += &format!(
        r#"for _ in $(seq {SSH_WAIT}); do
  running || {{ tail -n 20 serial.log >&2; die "$name stopped while booting"; }}
  banner=$(timeout 3 bash -c "exec 3<>/dev/tcp/127.0.0.1/$port && head -c 4 <&3" 2>/dev/null || true)
  if [ "$banner" = SSH- ]; then
    say "$name is up ({} CPUs, {} GiB), ssh on 127.0.0.1:$port of $(hostname)"
    exit 0
  fi
  sleep 1
done
tail -n 20 serial.log >&2
die "$name runs but its sshd did not answer in {SSH_WAIT} s"
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
  printf '%s\n' '{{"execute":"qmp_capabilities"}}' '{{"execute":"system_powerdown"}}' | socat -t 2 - "UNIX-CONNECT:$dir/qmp.sock" >/dev/null 2>&1 || true
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
