+++
title = "VM hosts"
description = "Declare a host that is a headless virtual machine on another one, boot it with one command, and apply it like any other host."
+++

A VM host is a host of your spec that runs as a virtual machine on another Linux machine with
KVM. It is a real system with its own init, services, users and kernel modules, so it suits a
test rig that must not touch a real desktop. fspec boots it; `fspec apply` makes it what the spec
says, the same as any [remote host](remote-hosts.md).

```starlark
fstestvm0 = host(
    name = "fstestvm0",
    platform = "x86_64-linux",
    system = tumbleweed(packages = ["NetworkManager", "openssh"]),
    users = [user("kyan", uid = 1000)],
    vm = vm("g815", port = 2222, cpus = 6, memory_gib = 8,
            authorized_keys = ["ssh-ed25519 AAAA... laptop"]),
)
```

## What it needs

The machine named in `on` needs `/dev/kvm` writable by your user, qemu, OVMF, curl, and for the
first start `mkfs.vfat` (dosfstools) and `mcopy` (mtools). On Tumbleweed: `qemu-x86`,
`qemu-ovmf-x86_64`, `qemu-tools`, `dosfstools`, `mtools`. `socat` lets `stop` power the guest off
cleanly; without it qemu is stopped after the wait.

Your ssh config reaches the guest through that machine, under the host's name:

```
Host fstestvm0
  HostName 127.0.0.1
  Port 2222
  User kyan
  ProxyJump g815
  StrictHostKeyChecking no
  UserKnownHostsFile /dev/null
```

The guest's host keys are new whenever its disk is, so this alias skips the known-hosts check;
the jump to `g815` is still checked.

## Boot it and apply it

```sh
fspec vm start fstestvm0     # first start: downloads the cloud image, creates the disk
fspec apply --host fstestvm0 # installs fspec there, then the spec
fspec vm ssh fstestvm0 -- systemctl is-active sshd
fspec vm stop fstestvm0
```

The first start downloads the distro's cloud image once into `~/.cache/fspec/vm/images/` on the
VM's machine and checks it against the checksum its project publishes. cloud-init gives the first
boot the host's name and its first user, with your keys and passwordless sudo. Everything else
comes from the spec: the first apply installs the fspec your lock pins, then the packages, files
and services. A spec that installs a kernel needs a `fspec vm stop` and `start` to boot it.

## Where it lives

Each VM is a directory on the VM's machine, `~/.local/share/fspec/vm/<host>/`: the disk, its
UEFI variables, the cloud-init seed, the serial console log (`serial.log`), the pid file and a QMP
socket. The VM keeps running after the ssh session that started it ends. To start over from the
image, stop it and delete the directory.

Hosts with their own `port` run at the same time; declare one per slot of a test rig.
