//! The one resolved runtime authority for a mounted provider.
//!
//! A pinned provider manifest and its parsed mount config are the complete
//! input to this type. Resolution happens once, before a provider instance is
//! constructed, and the resulting authority is shared by HTTP, Git, and WASI.

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use omnifs_caps::{AccessNeed, PreopenMode, PreopenedPath};
use omnifs_workspace::mounts::Spec;
use omnifs_workspace::provider::{ConfigMetadata, HostResourceBinding, ProviderManifest};
use reqwest::Url;

#[derive(Debug, thiserror::Error)]
pub enum AuthorityError {
    #[error("{kind} authority is invalid: {reason}")]
    Manifest {
        kind: &'static str,
        reason: &'static str,
    },
    #[error("{kind} authority requires config field `{field}`: {reason}")]
    Config {
        kind: &'static str,
        field: String,
        reason: &'static str,
    },
    #[error("{kind} authority is denied")]
    Denied { kind: &'static str },
    #[error("{kind} authority binding for config field `{field}` is invalid: {reason}")]
    Binding {
        kind: &'static str,
        field: String,
        reason: &'static str,
    },
    #[error("preopen authority for config field `{field}` is invalid: {reason}")]
    Preopen { field: String, reason: &'static str },
    #[error("preopen authority for config field `{field}` is unavailable")]
    PreopenUnavailable { field: String },
}

/// The approved transport returned after parsing and authorizing one provider
/// URL. Unix socket decoding is complete before this value reaches HTTP.
#[derive(Clone, Debug)]
pub(crate) enum ApprovedHttpTransport {
    Https(Url),
    Unix {
        socket: PathBuf,
        canonical_url: Url,
        request_url: Url,
    },
}

impl ApprovedHttpTransport {
    pub(crate) fn canonical_url(&self) -> &Url {
        match self {
            Self::Https(url)
            | Self::Unix {
                canonical_url: url, ..
            } => url,
        }
    }
}

/// Immutable host authority resolved for one mount.
pub struct RuntimeAuthority {
    domains: BTreeSet<String>,
    git_patterns: Vec<String>,
    unix_sockets: BTreeSet<PathBuf>,
    preopens: Vec<PreopenedPath>,
}

impl RuntimeAuthority {
    pub(crate) fn resolve(
        manifest: &ProviderManifest,
        spec: &Spec,
    ) -> Result<Arc<Self>, AuthorityError> {
        let config = spec.config_raw.as_ref();
        let metadata = manifest.config.as_ref();
        let mut domains = BTreeSet::new();
        let mut git_patterns = Vec::new();
        let mut needs_dynamic_domain = false;
        let mut needs_dynamic_socket = false;
        let mut needs_dynamic_preopen = false;

        for need in &manifest.capabilities {
            match need {
                AccessNeed::Domain { value, dynamic, .. } => {
                    if *dynamic {
                        needs_dynamic_domain = true;
                    } else {
                        domains.insert(normalize_domain(value, "domain")?);
                    }
                },
                AccessNeed::GitRepo { value, dynamic, .. } => {
                    if *dynamic {
                        return Err(AuthorityError::Manifest {
                            kind: "gitRepo",
                            reason: "dynamic Git patterns are unsupported",
                        });
                    }
                    if value.is_empty() {
                        return Err(AuthorityError::Manifest {
                            kind: "gitRepo",
                            reason: "pattern must not be empty",
                        });
                    }
                    git_patterns.push(value.clone());
                },
                AccessNeed::UnixSocket { dynamic, .. } => {
                    if *dynamic {
                        needs_dynamic_socket = true;
                    } else {
                        return Err(AuthorityError::Manifest {
                            kind: "unixSocket",
                            reason: "static Unix sockets must come from bound mount config",
                        });
                    }
                },
                AccessNeed::PreopenedPath { dynamic, .. } => {
                    if *dynamic {
                        needs_dynamic_preopen = true;
                    } else {
                        return Err(AuthorityError::Manifest {
                            kind: "preopenedPath",
                            reason: "static host preopens must come from bound mount config",
                        });
                    }
                },
            }
        }

        validate_resource_bindings(metadata, needs_dynamic_preopen, needs_dynamic_socket)?;

        if needs_dynamic_domain {
            let field = metadata.and_then(ConfigMetadata::domain_list_field).ok_or(
                AuthorityError::Config {
                    kind: "domain",
                    field: "domains".to_owned(),
                    reason: "no manifest-bound string-array field",
                },
            )?;
            let values = config
                .and_then(|value| value.get(field))
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| AuthorityError::Config {
                    kind: "domain",
                    field: field.to_owned(),
                    reason: "field must be a non-empty string array",
                })?;
            if values.is_empty() {
                return Err(AuthorityError::Config {
                    kind: "domain",
                    field: field.to_owned(),
                    reason: "field must be a non-empty string array",
                });
            }
            for value in values {
                let value = value.as_str().ok_or_else(|| AuthorityError::Config {
                    kind: "domain",
                    field: field.to_owned(),
                    reason: "field must contain only strings",
                })?;
                domains.insert(normalize_domain(value, "domain")?);
            }
        }

        let unix_sockets = metadata
            .into_iter()
            .flat_map(ConfigMetadata::host_resource_fields)
            .filter_map(|(field, field_metadata)| {
                matches!(field_metadata.binding, Some(HostResourceBinding::Socket)).then_some(field)
            })
            .map(|field| {
                let endpoint = config
                    .and_then(|value| value.get(field))
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| AuthorityError::Config {
                        kind: "unixSocket",
                        field: field.to_owned(),
                        reason: "field must contain a Unix endpoint",
                    })?;
                decode_socket_endpoint(endpoint).map_err(|reason| AuthorityError::Config {
                    kind: "unixSocket",
                    field: field.to_owned(),
                    reason,
                })
            })
            .collect::<Result<BTreeSet<_>, _>>()?;
        if needs_dynamic_socket && unix_sockets.is_empty() {
            return Err(AuthorityError::Config {
                kind: "unixSocket",
                field: "socket".to_owned(),
                reason: "no manifest-bound host-socket field",
            });
        }

        Ok(Arc::new(Self {
            domains,
            git_patterns,
            unix_sockets,
            preopens: resolve_preopens(manifest, spec)?,
        }))
    }

    pub(crate) fn approve_http(
        &self,
        raw_url: &str,
    ) -> Result<ApprovedHttpTransport, AuthorityError> {
        let parsed = Url::parse(raw_url).map_err(|_| AuthorityError::Denied { kind: "HTTP" })?;
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(AuthorityError::Denied { kind: "HTTP" });
        }
        match parsed.scheme() {
            "https" => {
                let host = parsed
                    .host_str()
                    .ok_or(AuthorityError::Denied { kind: "domain" })?;
                let normalized = normalize_request_host(host);
                if normalized
                    .parse::<IpAddr>()
                    .is_ok_and(|ip| is_private_or_link_local(&ip))
                    || !self.domains.contains(&normalized)
                {
                    return Err(AuthorityError::Denied { kind: "domain" });
                }
                Ok(ApprovedHttpTransport::Https(parsed))
            },
            "unix" => {
                let socket = decode_unix_url(&parsed)?;
                if !self.unix_sockets.contains(&socket) {
                    return Err(AuthorityError::Denied { kind: "unixSocket" });
                }
                let mut request_url = Url::parse("http://localhost/")
                    .expect("the fixed HTTP transport base URL is valid");
                request_url.set_path(parsed.path());
                request_url.set_query(parsed.query());
                request_url.set_fragment(parsed.fragment());
                Ok(ApprovedHttpTransport::Unix {
                    socket,
                    canonical_url: parsed,
                    request_url,
                })
            },
            _ => Err(AuthorityError::Denied { kind: "HTTP" }),
        }
    }

    pub(crate) fn check_git_url(&self, url: &str) -> Result<(), AuthorityError> {
        if self.git_patterns.iter().any(|pattern| {
            pattern
                .strip_suffix('*')
                .map_or_else(|| url == pattern, |prefix| url.starts_with(prefix))
        }) {
            Ok(())
        } else {
            Err(AuthorityError::Denied { kind: "gitRepo" })
        }
    }

    pub(crate) fn preopens(&self) -> &[PreopenedPath] {
        &self.preopens
    }

    /// Test-only constructor for executor tests that do not load provider
    /// metadata. Production resolution always goes through [`Self::resolve`].
    #[doc(hidden)]
    pub fn for_test(domains: &[&str], git_patterns: &[&str], unix_sockets: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            domains: domains.iter().map(|value| (*value).to_owned()).collect(),
            git_patterns: git_patterns
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
            unix_sockets: unix_sockets.iter().map(PathBuf::from).collect(),
            preopens: Vec::new(),
        })
    }
}

fn resolve_preopens(
    manifest: &ProviderManifest,
    spec: &Spec,
) -> Result<Vec<PreopenedPath>, AuthorityError> {
    let Some(metadata) = manifest.config.as_ref() else {
        return Ok(Vec::new());
    };
    let preopen_fields = metadata
        .host_resource_fields()
        .filter_map(|(field, field_metadata)| match field_metadata.binding? {
            HostResourceBinding::File { mode } => Some((field, mode)),
            HostResourceBinding::Socket => None,
        })
        .collect::<Vec<_>>();
    preopen_fields
        .into_iter()
        .map(|(field, mode)| {
            let configured = spec
                .config_raw
                .as_ref()
                .and_then(|value| value.get(field))
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| AuthorityError::Config {
                    kind: "preopenedPath",
                    field: field.to_owned(),
                    reason: "field must contain an absolute file path",
                })?;
            preopen_for(field, configured, mode)
        })
        .collect()
}

fn preopen_for(
    field: &str,
    configured: &str,
    mode: PreopenMode,
) -> Result<PreopenedPath, AuthorityError> {
    let configured = Path::new(configured);
    if !configured.is_absolute() {
        return Err(AuthorityError::Config {
            kind: "preopenedPath",
            field: field.to_owned(),
            reason: "field must contain an absolute file path",
        });
    }
    let guest = configured
        .parent()
        .filter(|path| *path != Path::new(""))
        .ok_or_else(|| AuthorityError::Preopen {
            field: field.to_owned(),
            reason: "file path has no parent directory",
        })?;
    if guest
        .components()
        .any(|component| component == Component::ParentDir)
    {
        return Err(AuthorityError::Preopen {
            field: field.to_owned(),
            reason: "guest path must not contain parent segments",
        });
    }
    let host = guest
        .canonicalize()
        .map_err(|_| AuthorityError::PreopenUnavailable {
            field: field.to_owned(),
        })?;
    if !host.is_dir() {
        return Err(AuthorityError::PreopenUnavailable {
            field: field.to_owned(),
        });
    }
    Ok(PreopenedPath {
        host: host.display().to_string(),
        guest: guest.display().to_string(),
        mode,
    })
}

fn validate_resource_bindings(
    metadata: Option<&ConfigMetadata>,
    needs_dynamic_preopen: bool,
    needs_dynamic_socket: bool,
) -> Result<(), AuthorityError> {
    let Some(metadata) = metadata else {
        if needs_dynamic_preopen {
            return Err(AuthorityError::Config {
                kind: "preopenedPath",
                field: "path".to_owned(),
                reason: "no manifest-bound host-file field",
            });
        }
        if needs_dynamic_socket {
            return Err(AuthorityError::Config {
                kind: "unixSocket",
                field: "socket".to_owned(),
                reason: "no manifest-bound host-socket field",
            });
        }
        return Ok(());
    };
    let mut has_host_file = false;
    let mut has_host_socket = false;
    for (field, field_metadata) in metadata.host_resource_fields() {
        match field_metadata.binding {
            Some(HostResourceBinding::File { .. }) => {
                has_host_file = true;
                if !needs_dynamic_preopen {
                    return Err(AuthorityError::Binding {
                        kind: "preopenedPath",
                        field: field.to_owned(),
                        reason: "bound host-file field requires a dynamic preopenedPath need",
                    });
                }
            },
            Some(HostResourceBinding::Socket) => {
                has_host_socket = true;
                if !needs_dynamic_socket {
                    return Err(AuthorityError::Binding {
                        kind: "unixSocket",
                        field: field.to_owned(),
                        reason: "bound host-socket field requires a dynamic unixSocket need",
                    });
                }
            },
            _ => {},
        }
    }
    if needs_dynamic_preopen && !has_host_file {
        return Err(AuthorityError::Config {
            kind: "preopenedPath",
            field: "path".to_owned(),
            reason: "no manifest-bound host-file field",
        });
    }
    if needs_dynamic_socket && !has_host_socket {
        return Err(AuthorityError::Config {
            kind: "unixSocket",
            field: "socket".to_owned(),
            reason: "no manifest-bound host-socket field",
        });
    }
    Ok(())
}

fn normalize_domain(value: &str, kind: &'static str) -> Result<String, AuthorityError> {
    if value.is_empty() || value == "*" || value.trim() != value {
        return Err(AuthorityError::Manifest {
            kind,
            reason: "domain must be a non-empty hostname without wildcard syntax",
        });
    }
    let parsed =
        Url::parse(&format!("https://{value}/")).map_err(|_| AuthorityError::Manifest {
            kind,
            reason: "domain must be a valid hostname",
        })?;
    if parsed.port().is_some()
        || parsed
            .host_str()
            .is_none_or(|host| !host.eq_ignore_ascii_case(value))
    {
        return Err(AuthorityError::Manifest {
            kind,
            reason: "domain must be a valid hostname without a port",
        });
    }
    Ok(normalize_request_host(
        parsed.host_str().unwrap_or_default(),
    ))
}

fn normalize_request_host(host: &str) -> String {
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase()
}

fn decode_socket_endpoint(endpoint: &str) -> Result<PathBuf, &'static str> {
    if raw_url_has_parent_segment(endpoint) {
        return Err("socket path must not contain parent segments");
    }
    let parsed = Url::parse(endpoint).map_err(|_| "field must contain a valid Unix endpoint")?;
    if parsed.scheme() != "unix" || !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("field must contain a Unix endpoint");
    }
    let path = if let Some(host) = parsed.host_str() {
        let bytes = hex::decode(host).map_err(|_| "Unix endpoint host must be hex-encoded")?;
        String::from_utf8(bytes).map_err(|_| "Unix endpoint host must decode to UTF-8")?
    } else {
        parsed.path().to_owned()
    };
    validate_socket_path(Path::new(&path))
}

fn raw_url_has_parent_segment(raw_url: &str) -> bool {
    let Some((_, authority_and_path)) = raw_url.split_once("://") else {
        return false;
    };
    let path = authority_and_path
        .split_once('/')
        .map_or("", |(_, path)| path)
        .split(['?', '#'])
        .next()
        .unwrap_or_default();
    path.split('/').any(|segment| segment == "..")
}

fn decode_unix_url(parsed: &Url) -> Result<PathBuf, AuthorityError> {
    let path = if let Some(host) = parsed.host_str() {
        let bytes = hex::decode(host).map_err(|_| AuthorityError::Denied { kind: "unixSocket" })?;
        String::from_utf8(bytes).map_err(|_| AuthorityError::Denied { kind: "unixSocket" })?
    } else {
        parsed.path().to_owned()
    };
    validate_socket_path(Path::new(&path))
        .map_err(|_| AuthorityError::Denied { kind: "unixSocket" })
}

fn validate_socket_path(path: &Path) -> Result<PathBuf, &'static str> {
    if !path.is_absolute() {
        return Err("socket path must be absolute");
    }
    if path
        .components()
        .any(|component| component == Component::ParentDir)
    {
        return Err("socket path must not contain parent segments");
    }
    Ok(path.to_path_buf())
}

fn is_private_or_link_local(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_loopback() || ip.is_private() || ip.is_link_local(),
        IpAddr::V6(ip) => ip.is_loopback() || ip.is_unicast_link_local() || ip.is_unique_local(),
    }
}

#[cfg(test)]
mod tests {
    use super::{AuthorityError, RuntimeAuthority, decode_socket_endpoint};
    use omnifs_caps::{AccessNeed, PreopenMode};
    use omnifs_workspace::ids::{ProviderId, ProviderMeta, ProviderName, ProviderRef};
    use omnifs_workspace::mounts::Spec;
    use omnifs_workspace::provider::{
        ConfigField, ConfigMetadata, ConfigType, HostResourceBinding, ProviderManifest,
    };

    fn spec() -> Spec {
        Spec {
            provider: ProviderRef {
                id: ProviderId::from_wasm_bytes(b"authority-test"),
                meta: ProviderMeta {
                    name: ProviderName::new("authority-test").unwrap(),
                    version: None,
                },
            },
            mount: "authority-test".to_owned(),
            revalidate: true,
            auth: None,
            limits: None,
            config_raw: None,
        }
    }

    fn manifest(capabilities: Vec<AccessNeed>, config: Option<ConfigMetadata>) -> ProviderManifest {
        ProviderManifest {
            id: "authority-test".to_owned(),
            display_name: "Authority test".to_owned(),
            description: None,
            provider: "authority-test.wasm".to_owned(),
            default_mount: "authority-test".to_owned(),
            version: None,
            wit_package: None,
            sdk_version: None,
            refresh_interval_secs: 0,
            capabilities,
            limits: Default::default(),
            auth: None,
            config,
        }
    }

    #[test]
    fn socket_endpoint_accepts_non_existing_absolute_parent_free_path() {
        let path = decode_socket_endpoint("unix:///run/omnifs/future.sock").unwrap();
        assert_eq!(path, std::path::Path::new("/run/omnifs/future.sock"));
    }

    #[test]
    fn socket_endpoint_rejects_parent_segments() {
        assert!(decode_socket_endpoint("unix:///run/../future.sock").is_err());
    }

    #[test]
    fn approved_http_transport_decodes_socket_once() {
        let authority = RuntimeAuthority::for_test(&[], &[], &["/run/omnifs.sock"]);
        let url = format!("unix://{}/v1/info", hex::encode("/run/omnifs.sock"));
        let transport = authority.approve_http(&url).unwrap();
        assert!(matches!(
            transport,
            super::ApprovedHttpTransport::Unix { request_url, .. }
                if request_url.scheme() == "http"
        ));
    }

    #[test]
    fn resource_binding_pairing_fails_closed_in_authority_resolution() {
        let dynamic_preopen = manifest(
            vec![AccessNeed::PreopenedPath {
                value: omnifs_caps::PreopenedPath {
                    host: "/data/file".to_owned(),
                    guest: "/data".to_owned(),
                    mode: PreopenMode::Ro,
                },
                why: "test".to_owned(),
                dynamic: true,
            }],
            None,
        );
        assert!(matches!(
            RuntimeAuthority::resolve(&dynamic_preopen, &spec()),
            Err(AuthorityError::Config {
                kind: "preopenedPath",
                ..
            })
        ));

        let unpaired_file = manifest(
            Vec::new(),
            Some(ConfigMetadata {
                fields: vec![ConfigField {
                    name: "path".to_owned(),
                    value_type: ConfigType::String,
                    required: true,
                    default: None,
                    description: None,
                    binding: Some(HostResourceBinding::File {
                        mode: PreopenMode::Ro,
                    }),
                }],
            }),
        );
        assert!(matches!(
            RuntimeAuthority::resolve(&unpaired_file, &spec()),
            Err(AuthorityError::Binding { kind: "preopenedPath", field, .. })
                if field == "path"
        ));

        let file = tempfile::NamedTempFile::new().unwrap();
        let mut paired_manifest = unpaired_file.clone();
        paired_manifest.capabilities = vec![AccessNeed::PreopenedPath {
            value: omnifs_caps::PreopenedPath {
                host: "/data/file".to_owned(),
                guest: "/data".to_owned(),
                mode: PreopenMode::Ro,
            },
            why: "test".to_owned(),
            dynamic: true,
        }];
        let mut paired_spec = spec();
        paired_spec.config_raw = Some(serde_json::json!({
            "path": file.path().display().to_string()
        }));
        let authority = RuntimeAuthority::resolve(&paired_manifest, &paired_spec).unwrap();
        assert_eq!(authority.preopens().len(), 1);
        let preopen = &authority.preopens()[0];
        let parent = file.path().parent().unwrap();
        assert_eq!(std::path::Path::new(&preopen.guest), parent);
        assert_eq!(
            std::path::Path::new(&preopen.host),
            parent.canonicalize().unwrap()
        );

        paired_spec.config_raw = None;
        assert!(matches!(
            RuntimeAuthority::resolve(&paired_manifest, &paired_spec),
            Err(AuthorityError::Config { kind: "preopenedPath", field, .. })
                if field == "path"
        ));

        let error = super::preopen_for(
            "path",
            "/private/authority-test/missing.db",
            PreopenMode::Ro,
        )
        .unwrap_err();
        let rendered = error.to_string();
        assert!(rendered.contains("config field `path`"));
        assert!(!rendered.contains("/private/authority-test/missing.db"));
    }
}
