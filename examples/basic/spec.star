# Three machines sharing two modules: a Tumbleweed laptop, a Bazzite workstation that boots a
# bootc image, and a MacBook. `fspec plan --host laptop` shows what one of them would get.
load("//modules/cli.star", "cli")
load("//modules/desktop.star", "desktop")

me = user("alex", uid = 1000, shell = "fish")

hosts = [
    host(
        name = "laptop",
        platform = "x86_64-linux",
        system = tumbleweed(packages = ["fish", "hyprland", "pipewire"]),
        users = [me],
        modules = [cli, desktop],
    ),
    host(
        name = "workstation",
        platform = "x86_64-linux",
        system = image(base = "ghcr.io/ublue-os/bazzite-nvidia:stable", kernel_args = ["iommu=force"]),
        users = [me],
        modules = [cli, desktop],
    ),
    host(
        name = "macbook",
        platform = "aarch64-darwin",
        users = [user("alex", uid = 501, shell = "fish")],
        modules = [cli, defaults("com.apple.dock", "autohide", True)],
    ),
]
