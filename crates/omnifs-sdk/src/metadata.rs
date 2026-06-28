//! Static provider metadata embedded in provider Wasm components.
//!
//! These are typed const data only. A provider's `Metadata` is evaluated by the
//! compiler into the `Provider::METADATA` associated const; the build-time
//! harvester reads it, converts it into the host's `ProviderManifest`, and
//! serializes that JSON into the `omnifs.provider-metadata.v1` custom section.
//! Nothing here serializes itself.

use crate::auth::Auth;
use crate::config_resource::ConfigMetadata;

/// Provider metadata contract.
pub trait Provider {
    const METADATA: Metadata;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Metadata {
    pub id: &'static str,
    pub display_name: &'static str,
    pub provider: &'static str,
    pub default_mount: &'static str,
    pub version: Option<&'static str>,
    pub capabilities: &'static [Need],
    pub auth: Option<Auth>,
    pub config: Option<ConfigMetadata>,
}

impl Metadata {
    #[must_use]
    pub const fn new(id: &'static str) -> Self {
        Self {
            id,
            display_name: id,
            provider: "",
            default_mount: id,
            version: None,
            capabilities: &[],
            auth: None,
            config: None,
        }
    }

    #[must_use]
    pub const fn display_name(mut self, display_name: &'static str) -> Self {
        self.display_name = display_name;
        self
    }

    #[must_use]
    pub const fn provider(mut self, provider: &'static str) -> Self {
        self.provider = provider;
        self
    }

    #[must_use]
    pub const fn mount(mut self, default_mount: &'static str) -> Self {
        self.default_mount = default_mount;
        self
    }

    #[must_use]
    pub const fn version(mut self, version: &'static str) -> Self {
        self.version = Some(version);
        self
    }

    #[must_use]
    pub const fn capabilities(mut self, capabilities: &'static [Need]) -> Self {
        self.capabilities = capabilities;
        self
    }

    #[must_use]
    pub const fn auth(mut self, auth: Auth) -> Self {
        self.auth = Some(auth);
        self
    }

    #[must_use]
    pub const fn config(mut self, config: Option<ConfigMetadata>) -> Self {
        self.config = config;
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Need {
    Domain {
        value: &'static str,
        why: &'static str,
        dynamic: bool,
    },
    GitRepo {
        value: &'static str,
        why: &'static str,
        dynamic: bool,
    },
    UnixSocket {
        value: &'static str,
        why: &'static str,
        dynamic: bool,
    },
    PreopenedPath {
        host: &'static str,
        guest: &'static str,
        why: &'static str,
        dynamic: bool,
    },
    MemoryMb {
        value: u32,
        why: &'static str,
        dynamic: bool,
    },
}

impl Need {
    #[must_use]
    pub const fn domain(value: &'static str, why: &'static str) -> Self {
        Self::Domain {
            value,
            why,
            dynamic: false,
        }
    }

    #[must_use]
    pub const fn git_repo(value: &'static str, why: &'static str) -> Self {
        Self::GitRepo {
            value,
            why,
            dynamic: false,
        }
    }

    #[must_use]
    pub const fn unix_socket_dynamic(why: &'static str) -> Self {
        Self::UnixSocket {
            value: DYNAMIC_PLACEHOLDER,
            why,
            dynamic: true,
        }
    }

    #[must_use]
    pub const fn preopened_path_dynamic(why: &'static str) -> Self {
        Self::PreopenedPath {
            host: DYNAMIC_PLACEHOLDER,
            guest: DYNAMIC_PLACEHOLDER,
            why,
            dynamic: true,
        }
    }

    #[must_use]
    pub const fn memory_mb(value: u32, why: &'static str) -> Self {
        Self::MemoryMb {
            value,
            why,
            dynamic: false,
        }
    }
}

pub const DYNAMIC_PLACEHOLDER: &str = "resolved from config at mount-start";
