/// How a frontend is delivered to the shared namespace.
///
/// The launcher supplies this identity to the frontend, which carries it in
/// every wire handshake.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum FrontendRuntime {
    /// Launched as a native host frontend.
    Host,
    /// Launched in the workspace's Docker frontend container.
    Docker,
    /// Launched in the workspace's libkrun frontend guest.
    Libkrun,
}

impl FrontendRuntime {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Docker => "docker",
            Self::Libkrun => "libkrun",
        }
    }
}

impl std::fmt::Display for FrontendRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for FrontendRuntime {
    type Err = ParseFrontendRuntimeError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "host" => Ok(Self::Host),
            "docker" => Ok(Self::Docker),
            "libkrun" => Ok(Self::Libkrun),
            _ => Err(ParseFrontendRuntimeError(value.to_owned())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown frontend runtime `{0}`; expected host, docker, or libkrun")]
pub struct ParseFrontendRuntimeError(String);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_runtime_names_and_rejects_unknown_values() {
        assert_eq!("host".parse(), Ok(FrontendRuntime::Host));
        assert_eq!("docker".parse(), Ok(FrontendRuntime::Docker));
        assert_eq!("libkrun".parse(), Ok(FrontendRuntime::Libkrun));
        assert_eq!(
            "vm".parse::<FrontendRuntime>().unwrap_err().to_string(),
            "unknown frontend runtime `vm`; expected host, docker, or libkrun"
        );
    }
}
