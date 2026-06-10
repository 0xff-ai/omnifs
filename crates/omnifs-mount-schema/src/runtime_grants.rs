use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::Path;
use utoipa::ToSchema;

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq, ToSchema)]
pub struct PreopenedPath {
    pub host: String,
    pub guest: String,
    #[serde(default)]
    pub mode: PreopenMode,
}

#[derive(
    Clone, Copy, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq, ToSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum PreopenMode {
    #[default]
    Ro,
    Rw,
}

/// Runtime capability grants for a mounted provider instance.
///
/// This type is user-authored in mount JSON configs and controls what
/// sandbox capabilities are granted. It is distinct from `CapabilityEntry`
/// which is the provider-manifest declaration of what a provider needs.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq, ToSchema)]
pub struct ProviderCapabilities {
    #[serde(default)]
    pub domains: Option<Vec<String>>,
    #[serde(default)]
    pub git_repos: Option<Vec<String>>,
    #[serde(default)]
    pub unix_sockets: Option<Vec<String>>,
    #[serde(default)]
    pub preopened_paths: Option<Vec<PreopenedPath>>,
    #[serde(default)]
    pub max_memory_mb: Option<u32>,
    #[serde(default)]
    pub max_fetch_blob_bytes: Option<u64>,
    #[serde(default)]
    pub max_read_blob_bytes: Option<u64>,
}

impl ProviderCapabilities {
    /// Grant the unix socket configured by a provider endpoint.
    ///
    /// Non-unix endpoints do not imply a socket grant. Existing socket grants
    /// are normalized to absolute paths so placeholder labels from provider
    /// metadata do not reach the runtime allowlist.
    pub fn grant_configured_unix_socket(
        &mut self,
        endpoint: &str,
    ) -> Result<(), UnixSocketEndpointError> {
        let Some(socket) = ConfiguredUnixSocket::parse(endpoint)? else {
            return Ok(());
        };
        let socket_path = socket.as_str();

        let sockets = self.unix_sockets.get_or_insert_with(Vec::new);
        sockets.retain(|socket| Path::new(socket).is_absolute());
        if !sockets.iter().any(|socket| socket == socket_path) {
            sockets.push(socket.into_string());
        }
        Ok(())
    }
}

struct ConfiguredUnixSocket(String);

impl ConfiguredUnixSocket {
    fn parse(endpoint: &str) -> Result<Option<Self>, UnixSocketEndpointError> {
        let Some(raw) = endpoint.strip_prefix("unix://") else {
            return Ok(None);
        };
        if raw.starts_with('/') {
            return Self::new(raw.to_string(), endpoint).map(Some);
        }

        let host = raw
            .split('/')
            .next()
            .filter(|host| !host.is_empty())
            .ok_or(UnixSocketEndpointError::MissingHost {
                endpoint: endpoint.to_string(),
            })?;
        let bytes = hex::decode(host).map_err(|source| UnixSocketEndpointError::HexHost {
            endpoint: endpoint.to_string(),
            source,
        })?;
        let socket =
            String::from_utf8(bytes).map_err(|source| UnixSocketEndpointError::Utf8Host {
                endpoint: endpoint.to_string(),
                source,
            })?;
        Self::new(socket, endpoint).map(Some)
    }

    fn new(socket: String, endpoint: &str) -> Result<Self, UnixSocketEndpointError> {
        if !Path::new(&socket).is_absolute() {
            return Err(UnixSocketEndpointError::NonAbsolute {
                endpoint: endpoint.to_string(),
                socket,
            });
        }
        Ok(Self(socket))
    }

    fn as_str(&self) -> &str {
        &self.0
    }

    fn into_string(self) -> String {
        self.0
    }
}

#[derive(Debug, thiserror::Error)]
pub enum UnixSocketEndpointError {
    #[error("configured unix endpoint `{endpoint}` is missing a socket host")]
    MissingHost { endpoint: String },
    #[error(
        "configured unix endpoint `{endpoint}` resolved to non-absolute socket path `{socket}`"
    )]
    NonAbsolute { endpoint: String, socket: String },
    #[error("configured unix endpoint `{endpoint}` host is not hex-encoded: {source}")]
    HexHost {
        endpoint: String,
        source: hex::FromHexError,
    },
    #[error("configured unix endpoint `{endpoint}` host decodes to non-UTF-8 path: {source}")]
    Utf8Host {
        endpoint: String,
        source: std::string::FromUtf8Error,
    },
}
