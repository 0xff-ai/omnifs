//! Compile-time build-channel identity shared by every binary-level
//! consumer: the CLI's `version` command and the daemon's runtime image
//! resolvers both need this fact without depending on each other.

/// Whether this binary was produced by the release packaging lane
/// (`OMNIFS_RELEASE` set at compile time) or a local/dev build. Release
/// binaries default to the registry image for their version; dev binaries
/// default to the locally built dev image and never pull.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildChannel {
    Release,
    Dev,
}

impl BuildChannel {
    /// Why a missing registry-less image is never pulled. Only a dev binary
    /// defaults to a local image, so release errors must not call it a dev build.
    pub const fn pull_refusal_reason(self) -> &'static str {
        match self {
            Self::Dev => {
                "this omnifs binary is a dev build; it uses the locally built filesystem image \
                 and never pulls from a registry"
            },
            Self::Release => {
                "registry-less image references are local build products; omnifs never pulls \
                 them from a registry"
            },
        }
    }

    pub const fn word(self) -> &'static str {
        match self {
            Self::Dev => "dev",
            Self::Release => "release",
        }
    }

    pub const fn version_suffix(self) -> &'static str {
        match self {
            Self::Dev => " (dev build)",
            Self::Release => "",
        }
    }
}

pub const BUILD_CHANNEL: BuildChannel = match option_env!("OMNIFS_RELEASE") {
    Some(_) => BuildChannel::Release,
    None => BuildChannel::Dev,
};
