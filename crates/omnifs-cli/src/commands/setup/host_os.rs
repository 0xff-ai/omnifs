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
    std::fs::read_to_string("/proc/version").is_ok_and(|content| wsl_marker_present(&content))
}

pub fn name(os: HostOs) -> &'static str {
    match os {
        HostOs::MacOs => "macOS",
        HostOs::LinuxNative => "Linux",
        HostOs::LinuxWsl => "Linux (WSL)",
        HostOs::Unsupported => "unsupported",
    }
}

pub fn explain_runtime(os: HostOs) -> String {
    let lead = "omnifs runs a host-native daemon by default. Docker is optional; \
        when selected, Docker runs the Linux FUSE frontend inside the container.";

    let per_os = match os {
        HostOs::MacOs => {
            "On macOS, the native daemon serves a loopback NFS mount at a user-owned host path."
        },
        HostOs::LinuxNative => {
            "On Linux, the native daemon serves the kernel FUSE frontend directly."
        },
        HostOs::LinuxWsl => {
            "Inside WSL2, run setup from your WSL terminal. Native mode uses the Linux FUSE frontend in that distro."
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
