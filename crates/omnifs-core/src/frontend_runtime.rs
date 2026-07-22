/// How a frontend is delivered to the shared namespace.
///
/// The host assigns this from listener ownership. A connecting guest never
/// reports its own runtime.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum FrontendRuntime {
    /// Attached over the fixed local Unix domain socket.
    Host,
    /// Attached over the TCP listener used by Docker.
    Docker,
    /// Attached over the Unix-domain vsock proxy used by libkrun.
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
