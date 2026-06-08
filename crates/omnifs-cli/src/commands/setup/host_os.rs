/// Detected host operating system for the omnifs setup command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostOs {
    MacOs,
    LinuxNative,
    LinuxWsl,
    Unsupported,
}

fn wsl_marker_present(content: &str) -> bool {
    let lower = content.to_lowercase();
    lower.contains("microsoft") || lower.contains("wsl")
}

pub fn detect() -> HostOs {
    match std::env::consts::OS {
        "macos" => HostOs::MacOs,
        "linux" => {
            if is_wsl() {
                HostOs::LinuxWsl
            } else {
                HostOs::LinuxNative
            }
        },
        _ => HostOs::Unsupported,
    }
}

fn is_wsl() -> bool {
    std::fs::read_to_string("/proc/version")
        .map(|content| wsl_marker_present(&content))
        .unwrap_or(false)
}

pub fn name(os: HostOs) -> &'static str {
    match os {
        HostOs::MacOs => "macOS",
        HostOs::LinuxNative => "Linux",
        HostOs::LinuxWsl => "Linux (WSL)",
        HostOs::Unsupported => "unsupported",
    }
}

pub fn explain_alpha_runtime(os: HostOs) -> String {
    let lead = "omnifs can run either natively or through Docker. Native mode runs the \
        provider host on this machine and mounts a local filesystem frontend; Docker mode \
        keeps the older containerized runtime available as a fallback.";

    let per_os = match os {
        HostOs::MacOs => {
            "On macOS, native mode uses the NFSv4 loopback frontend so the mount is visible \
            to host shells and Finder. Docker mode remains available when you want the older \
            Linux-container runtime."
        },
        HostOs::LinuxNative => {
            "On Linux, native mode uses the FUSE frontend. Docker mode remains available for \
            contributors and environments where /dev/fuse or local privileges are awkward."
        },
        HostOs::LinuxWsl => {
            "Inside WSL2, native mode is used only when the distro exposes a usable FUSE \
            device. Docker mode remains the fallback. Run setup from your WSL terminal, not \
            from cmd.exe or PowerShell."
        },
        HostOs::Unsupported => {
            "Your platform is not yet supported by omnifs. \
            Tracked platforms are macOS, Linux, and WSL2."
        },
    };

    format!("{lead}\n\n{per_os}")
}

pub fn docker_install_hint(os: HostOs) -> &'static str {
    match os {
        HostOs::MacOs => {
            "Install Docker Desktop (https://docs.docker.com/desktop/install/mac/) \
            or OrbStack (https://orbstack.dev). \
            Either exposes a Docker socket the CLI will pick up."
        },
        HostOs::LinuxNative => {
            "Install Docker Engine (https://docs.docker.com/engine/install/) \
            and ensure your user is in the `docker` group, \
            or install Podman with docker-socket compatibility."
        },
        HostOs::LinuxWsl => {
            "Install Docker Desktop for Windows and enable WSL2 integration for this distro, \
            or install Docker Engine inside your WSL distro."
        },
        HostOs::Unsupported => "omnifs needs macOS, Linux, or WSL2.",
    }
}
