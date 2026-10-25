# A console system with networking and sudo. The other presets build on it.

NETWORK = {
    "fedora": ["NetworkManager"],
    "arch": ["networkmanager"],
    "debian": ["network-manager"],
    "ubuntu": ["network-manager"],
    "void": ["NetworkManager", "dbus"],
    "aerynos": ["networkmanager"],
}

NETWORK_SERVICES = {
    "void": ["dbus", "NetworkManager"],
}

SUDO = {"aerynos": ["sudo-rs"]}
EDITOR = ["nano"]

# locale-gen, which builds the host's locale, comes with this package.
LOCALES = {"debian": ["locales"], "ubuntu": ["locales"]}

def distro_system(distro, packages):
    if distro == "fedora":
        return fedora(packages = packages)
    if distro == "arch":
        return arch(packages = packages)
    if distro == "debian":
        return debian(packages = packages)
    if distro == "ubuntu":
        return ubuntu(packages = packages)
    if distro == "void":
        return void(packages = packages)
    if distro == "aerynos":
        return aerynos(packages = packages)
    fail("minimal has no system for " + distro)

def system(distro, desktop = None, extra = []):
    return distro_system(distro, NETWORK[distro] + SUDO.get(distro, ["sudo"]) + EDITOR + LOCALES.get(distro, []) + extra)

def modules(distro, desktop = None):
    found = [service(name) for name in NETWORK_SERVICES.get(distro, ["NetworkManager"])]
    # These ship without a wheel rule.
    if distro in ["arch", "void"]:
        found.append(file("/etc/sudoers.d/wheel", content = "%wheel ALL=(ALL:ALL) ALL\n", perms = "0440"))
    return found
