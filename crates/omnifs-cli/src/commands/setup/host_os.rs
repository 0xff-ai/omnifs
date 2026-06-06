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
    let lead = "omnifs runs inside a Docker container during alpha. This is temporary. \
        The host CLI talks to a daemon inside the container, which mounts a FUSE filesystem \
        and proxies it back out to the host.";

    let per_os = match os {
        HostOs::MacOs => {
            "On macOS, native FUSE integration needs a kernel extension we have not shipped yet. \
            The container hosts a Linux FUSE stack and we project the mount back through a shared volume."
        },
        HostOs::LinuxNative => {
            "On Linux, the container model gives one binary path for everyone while the runtime \
            stabilises. Post-alpha, native Linux will skip the container entirely."
        },
        HostOs::LinuxWsl => {
            "Inside WSL2, the container approach mirrors what we do on macOS and native Linux. \
            Run setup from your WSL terminal, not from cmd.exe or PowerShell."
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
