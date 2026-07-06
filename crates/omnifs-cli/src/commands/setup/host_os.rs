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

    fn is_wsl() -> bool {
        std::fs::read_to_string("/proc/version")
            .is_ok_and(|content| Self::wsl_marker_present(&content))
    }

    fn wsl_marker_present(content: &str) -> bool {
        let lower = content.to_lowercase();
        lower.contains("microsoft") || lower.contains("wsl")
    }
}
