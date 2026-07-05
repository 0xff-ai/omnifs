/// Detected host operating system for the omnifs setup command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostOs {
    MacOs,
    LinuxNative,
    LinuxWsl,
    Unsupported,
}

impl HostOs {
    pub fn detect() -> Self {
        match std::env::consts::OS {
            "macos" => Self::MacOs,
            "linux" => {
                if Self::is_wsl() {
                    Self::LinuxWsl
                } else {
                    Self::LinuxNative
                }
            },
            _ => Self::Unsupported,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::MacOs => "macOS",
            Self::LinuxNative => "Linux",
            Self::LinuxWsl => "Linux (WSL)",
            Self::Unsupported => "unsupported",
        }
    }

    pub fn explain_runtime(self) -> String {
        let lead = "omnifs can run its daemon two ways: host-native, or inside a Docker \
            container running the Linux FUSE frontend. You'll pick a default next; \
            re-run setup to change it, or override it per launch with \
            `omnifs up --runtime <docker|native>`.";

        let per_os = match self {
            Self::MacOs => {
                "On macOS the default is Docker. Host-native serves a loopback NFS mount \
                at a user-owned host path and is experimental."
            },
            Self::LinuxNative => {
                "On Linux the default is host-native, serving the kernel FUSE frontend \
                directly; Docker is also available."
            },
            Self::LinuxWsl => {
                "Inside WSL2 the default is host-native FUSE; run setup from your WSL \
                terminal. Docker is also available."
            },
            Self::Unsupported => {
                "Your platform is not yet supported by omnifs. \
                Tracked platforms are macOS, Linux, and WSL2."
            },
        };

        format!("{lead}\n\n{per_os}")
    }

    pub fn docker_install_hint(self) -> &'static str {
        match self {
            Self::MacOs => {
                "Install Docker Desktop (https://docs.docker.com/desktop/install/mac/) \
                or OrbStack (https://orbstack.dev). \
                Either exposes a Docker socket the CLI will pick up."
            },
            Self::LinuxNative => {
                "Install Docker Engine (https://docs.docker.com/engine/install/) \
                and ensure your user is in the `docker` group, \
                or install Podman with docker-socket compatibility."
            },
            Self::LinuxWsl => {
                "Install Docker Desktop for Windows and enable WSL2 integration for this distro, \
                or install Docker Engine inside your WSL distro."
            },
            Self::Unsupported => "omnifs needs macOS, Linux, or WSL2.",
        }
    }

    fn is_wsl() -> bool {
        std::fs::read_to_string("/proc/version")
            .is_ok_and(|content| Self::wsl_marker_present(&content))
    }

    fn wsl_marker_present(content: &str) -> bool {
        let lower = content.to_lowercase();
        lower.contains("microsoft") || lower.contains("wsl")
    }
}
