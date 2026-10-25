# SSH, a firewall and automatic security updates. No desktop.

load("//modules/minimal.star", base_modules = "modules", base_system = "system")

SSH = {
    "fedora": ["openssh-server"],
    "arch": ["openssh"],
    "debian": ["openssh-server"],
    "ubuntu": ["openssh-server"],
    "void": ["openssh"],
}

SSH_SERVICE = {"debian": "ssh", "ubuntu": "ssh"}

# firewalld's default zone lets ssh in. Void's repo has no firewalld, so Void uses an
# nftables ruleset that does the same.
FIREWALL = {
    "fedora": ["firewalld"],
    "arch": ["firewalld"],
    "debian": ["firewalld"],
    "ubuntu": ["firewalld"],
    "void": ["nftables"],
}

# Rolling distros get no unattended upgrades: a partial upgrade breaks them, so updating
# stays a deliberate `pacman -Syu` or `xbps-install -Su`.
UPDATES = {
    "fedora": ["dnf5-plugin-automatic"],
    "debian": ["unattended-upgrades"],
    "ubuntu": ["unattended-upgrades"],
}

NFTABLES = """flush ruleset

table inet filter {
    chain input {
        type filter hook input priority filter; policy drop;
        ct state established,related accept
        iif lo accept
        meta l4proto { icmp, ipv6-icmp } accept
        udp dport 68 accept
        tcp dport 22 accept
    }
    chain forward {
        type filter hook forward priority filter; policy drop;
    }
}
"""

APT_PERIODIC = """APT::Periodic::Update-Package-Lists "1";
APT::Periodic::Unattended-Upgrade "1";
"""

def system(distro, desktop = None):
    return base_system(distro, extra = SSH[distro] + FIREWALL[distro] + UPDATES.get(distro, []))

def modules(distro, desktop = None):
    found = base_modules(distro) + [
        service(SSH_SERVICE.get(distro, "sshd")),
        service("nftables" if distro == "void" else "firewalld"),
    ]
    if distro == "void":
        found.append(file("/etc/nftables.conf", content = NFTABLES))
    if distro == "fedora":
        found += [
            file("/etc/dnf/automatic.conf", format = "ini", content = {"commands": {"upgrade_type": "security", "apply_updates": True}}),
            service("dnf5-automatic.timer"),
        ]
    if distro in ["debian", "ubuntu"]:
        found.append(file("/etc/apt/apt.conf.d/20auto-upgrades", content = APT_PERIODIC))
    return found
