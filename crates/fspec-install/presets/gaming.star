# The desktop plus Steam, Proton, GameMode and MangoHud.

load("//modules/desktop.star", desktop_modules = "modules", desktop_system = "system")

# The Flathub Steam talks to the host's gamemoded over D-Bus, so GameMode comes from the distro.
# MangoHud has to live inside Steam's sandbox, so it comes from Flathub as a Vulkan layer.
# Steam downloads Proton itself, and its Flatpak needs no RPM Fusion or multilib.
GAMEMODE = ["gamemode"]

def system(distro, desktop = None):
    return desktop_system(distro, desktop, extra = GAMEMODE)

def modules(distro, desktop = None):
    return desktop_modules(distro, desktop) + [
        pkg("com.valvesoftware.Steam", source = flatpak("com.valvesoftware.Steam"), mode = "self_managed"),
        pkg(
            "org.freedesktop.Platform.VulkanLayer.MangoHud",
            source = flatpak("org.freedesktop.Platform.VulkanLayer.MangoHud"),
            mode = "self_managed",
        ),
    ]
