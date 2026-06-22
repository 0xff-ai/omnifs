//! Resolving a dynamic grant to its concrete value at mount init. Today the
//! only dynamic capability is a unix socket derived from the mount's endpoint
//! config; the host calls [`endpoint_socket`] when a mount grants
//! `unix_sockets: { "dynamic": true }` and feeds the result into the runtime
//! [`Allowlist`](crate::Allowlist).

/// The absolute unix socket path a `unix://` endpoint config points at, or
/// `None` for a non-unix endpoint. The host segment is either a literal
/// absolute path (`unix:///var/run/docker.sock`) or the hex-encoded socket path
/// (`unix://<hex>`, the `hyperlocal` convention).
pub fn endpoint_socket(endpoint: &str) -> Result<Option<String>, EndpointError> {
    let Some(raw) = endpoint.strip_prefix("unix://") else {
        return Ok(None);
    };
    if raw.starts_with('/') {
        return absolute(raw.to_string(), endpoint).map(Some);
    }
    let host = raw
        .split('/')
        .next()
        .filter(|host| !host.is_empty())
        .ok_or_else(|| EndpointError::MissingHost {
            endpoint: endpoint.to_string(),
        })?;
    let bytes = hex::decode(host).map_err(|source| EndpointError::HexHost {
        endpoint: endpoint.to_string(),
        source,
    })?;
    let socket = String::from_utf8(bytes).map_err(|source| EndpointError::Utf8Host {
        endpoint: endpoint.to_string(),
        source,
    })?;
    absolute(socket, endpoint).map(Some)
}

fn absolute(socket: String, endpoint: &str) -> Result<String, EndpointError> {
    if std::path::Path::new(&socket).is_absolute() {
        Ok(socket)
    } else {
        Err(EndpointError::NonAbsolute {
            endpoint: endpoint.to_string(),
            socket,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EndpointError {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_unix_endpoint_has_no_socket() {
        assert_eq!(endpoint_socket("https://api.example.com").unwrap(), None);
    }

    #[test]
    fn literal_absolute_socket_resolves() {
        assert_eq!(
            endpoint_socket("unix:///var/run/docker.sock").unwrap(),
            Some("/var/run/docker.sock".to_string())
        );
    }

    #[test]
    fn hex_host_socket_resolves() {
        let endpoint = format!("unix://{}/v1.43/info", hex::encode("/var/run/docker.sock"));
        assert_eq!(
            endpoint_socket(&endpoint).unwrap(),
            Some("/var/run/docker.sock".to_string())
        );
    }

    #[test]
    fn relative_socket_is_rejected() {
        assert!(matches!(
            endpoint_socket("unix://relative/path"),
            Err(EndpointError::HexHost { .. }) | Err(EndpointError::NonAbsolute { .. })
        ));
    }
}
