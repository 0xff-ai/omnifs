//! Mount materialization: turn an authored `Spec` into a runtime-ready one.
//!
//! Shared by the CLI (for the pre-launch credential/capability preflight) and
//! the daemon (to reconcile `mounts/*.json` into the registry). The steps, in
//! order: read the pinned manifest's capability needs, check that the spec's
//! grants satisfy them, then canonicalize preopens. The spec already carries
//! its provider-manifest defaults (baked in at creation), so materialization
//! reads the manifest but never mutates the spec's auth or config. A preopen
//! whose host equals its guest is runtime-native (provided in the runtime's
//! own filesystem, e.g. a dev fixture bind) and passes through untouched.
//! Otherwise the preopen host path is canonicalized in place: the runtime
//! (daemon or provider sandbox) opens the real host directory directly.

use std::path::Path;

use crate::provider::{Catalog, ConfigMetadata, is_hostname_only};
use omnifs_caps::Grant;

use crate::mounts::{Spec, SpecError, pinned_manifest};

#[derive(Debug, thiserror::Error)]
pub enum MaterializeError {
    #[error("apply provider metadata: {0}")]
    Metadata(#[source] SpecError),
    #[error("canonicalize preopen `{path}`: {source}")]
    PreopenPath {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("preopen `{0}` is not a directory")]
    PreopenNotDir(String),
    /// The mount spec grants fewer capabilities than the pinned provider's
    /// manifest declares it needs, so the provider would be denied a declared
    /// callout at its first request. The daemon refuses to start an
    /// under-granted mount.
    #[error(
        "mount `{mount}` under-grants provider `{provider}`: the spec is missing \
         manifest-required {missing}; re-run `omnifs init {provider} --as {mount}` \
         or add the capability to the mount spec"
    )]
    MissingCapabilities {
        mount: String,
        provider: String,
        missing: String,
    },
    /// A dynamic unix-socket grant whose `endpoint` config does not resolve to a
    /// socket path. The runtime allowlist would be empty, so the provider would
    /// be denied at its first callout; the daemon refuses to start the mount and
    /// points at the endpoint config instead.
    #[error(
        "mount `{mount}` has a dynamic unix-socket grant that does not resolve: \
         {detail}; fix the mount's `endpoint`"
    )]
    UnresolvedDynamicSocket { mount: String, detail: String },
    /// A dynamic domain grant whose `domains` config does not resolve to at
    /// least one concrete hostname. The runtime allowlist would be empty, so
    /// the provider would be denied at its first callout.
    #[error(
        "mount `{mount}` has a dynamic domain grant that does not resolve: \
         {detail}; fix the mount's `domains` config"
    )]
    UnresolvedDynamicDomains { mount: String, detail: String },
}

/// Materialize `spec` against `catalog`: check its grants against the pinned
/// manifest's declared needs, then canonicalize preopen host paths in place so
/// the runtime can open them directly.
pub fn materialize(mut spec: Spec, catalog: &Catalog) -> Result<Spec, MaterializeError> {
    // Required-capabilities check: the spec's grants must satisfy every
    // capability the pinned manifest declares the provider needs, so an
    // under-granted mount fails here rather than at the provider's first denied
    // callout. Over-granting beyond the manifest is allowed; the over-grant
    // check is deliberately not enforced (docs/future/provider-contract-versioning.md).
    if let Some(manifest) = pinned_manifest(catalog, &spec).map_err(MaterializeError::Metadata)? {
        let missing = spec
            .capabilities
            .clone()
            .unwrap_or_default()
            .satisfies(&manifest.capabilities);
        if !missing.is_empty() {
            return Err(MaterializeError::MissingCapabilities {
                mount: spec.mount.clone(),
                provider: spec.provider.meta.name.to_string(),
                missing: missing
                    .iter()
                    .map(|cap| format!("{} `{}`", cap.kind, cap.value))
                    .collect::<Vec<_>>()
                    .join(", "),
            });
        }
        check_dynamic_domains(&spec, manifest.config.as_ref())?;
        check_dynamic_socket(&spec, manifest.config.as_ref())?;
    }

    canonicalize_preopens(&mut spec)?;

    Ok(spec)
}

/// Verify that a dynamic unix-socket grant resolves from the config field the
/// provider marks as a host socket. A dynamic grant passes the
/// required-capabilities check (a dynamic grant satisfies a dynamic need), but
/// the runtime allowlist is built by resolving that field's value; if it does
/// not resolve, the provider is silently denied at its first callout. Resolving
/// here turns that into a clear, fixable mount-start error. A dynamic preopen
/// resolves from a host-file field at instance creation instead, so it is not
/// checked here.
fn check_dynamic_socket(
    spec: &Spec,
    metadata: Option<&ConfigMetadata>,
) -> Result<(), MaterializeError> {
    let is_dynamic = spec
        .capabilities
        .as_ref()
        .and_then(|caps| caps.unix_sockets.as_ref())
        .is_some_and(|grant| matches!(grant, Grant::Dynamic(_)));
    if !is_dynamic {
        return Ok(());
    }
    let Some(field) = metadata.and_then(ConfigMetadata::host_socket_field) else {
        return Err(MaterializeError::UnresolvedDynamicSocket {
            mount: spec.mount.clone(),
            detail: "no config field is marked as a host socket".to_string(),
        });
    };
    let endpoint = spec
        .config_raw
        .as_ref()
        .and_then(|config| config.get(field))
        .and_then(serde_json::Value::as_str);
    let detail = match endpoint {
        None => format!("no `{field}` config is set"),
        Some(endpoint) => match omnifs_caps::endpoint_socket(endpoint) {
            Ok(Some(_)) => return Ok(()),
            Ok(None) => format!("endpoint `{endpoint}` is not a unix socket"),
            Err(error) => error.to_string(),
        },
    };
    Err(MaterializeError::UnresolvedDynamicSocket {
        mount: spec.mount.clone(),
        detail,
    })
}

fn check_dynamic_domains(
    spec: &Spec,
    metadata: Option<&ConfigMetadata>,
) -> Result<(), MaterializeError> {
    let is_dynamic = spec
        .capabilities
        .as_ref()
        .and_then(|caps| caps.domains.as_ref())
        .is_some_and(|grant| matches!(grant, Grant::Dynamic(_)));
    if !is_dynamic {
        return Ok(());
    }
    let Some(field) = metadata.and_then(ConfigMetadata::domain_list_field) else {
        return Err(MaterializeError::UnresolvedDynamicDomains {
            mount: spec.mount.clone(),
            detail: "no config field named `domains` is a string array".to_string(),
        });
    };
    let domains = config_string_array(spec, field);
    let detail = match domains {
        None => format!("no `{field}` config is set"),
        Some([]) => format!("`{field}` is empty"),
        Some(domains) => {
            if let Some(domain) = domains.iter().find(|domain| !is_dynamic_domain(domain)) {
                format!("invalid domain `{domain}`")
            } else {
                return Ok(());
            }
        },
    };
    Err(MaterializeError::UnresolvedDynamicDomains {
        mount: spec.mount.clone(),
        detail,
    })
}

fn config_string_array<'a>(spec: &'a Spec, field: &str) -> Option<&'a [serde_json::Value]> {
    spec.config_raw
        .as_ref()
        .and_then(|config| config.get(field))
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
}

fn is_dynamic_domain(value: &serde_json::Value) -> bool {
    value.as_str().is_some_and(is_hostname_only)
}

#[cfg(test)]
mod dynamic_socket_tests {
    use super::*;
    use crate::ids::{ProviderId, ProviderMeta, ProviderName, ProviderRef};

    fn dynamic_socket_spec(endpoint: Option<&str>) -> Spec {
        let provider = ProviderRef {
            id: ProviderId::from_wasm_bytes(b"k8s"),
            meta: ProviderMeta {
                name: ProviderName::new("k8s").unwrap(),
                version: None,
            },
        };
        let mut value = serde_json::json!({
            "provider": provider,
            "mount": "k8s",
            "capabilities": { "unix_sockets": { "dynamic": true } },
        });
        if let Some(endpoint) = endpoint {
            value["config"] = serde_json::json!({ "endpoint": endpoint });
        }
        serde_json::from_value(value).expect("spec parses")
    }

    fn socket_metadata() -> ConfigMetadata {
        serde_json::from_value(serde_json::json!({
            "fields": [{
                "name": "endpoint",
                "type": { "kind": "string" },
                "binding": { "kind": "socket" }
            }]
        }))
        .expect("config metadata parses")
    }

    #[test]
    fn dynamic_socket_validation_errors() {
        let metadata = socket_metadata();
        assert!(
            check_dynamic_socket(
                &dynamic_socket_spec(Some("unix:///run/omnifs/k8s.sock")),
                Some(&metadata)
            )
            .is_ok()
        );
        assert!(matches!(
            check_dynamic_socket(&dynamic_socket_spec(None), Some(&metadata)),
            Err(MaterializeError::UnresolvedDynamicSocket { .. })
        ));
        assert!(matches!(
            check_dynamic_socket(
                &dynamic_socket_spec(Some("https://example.com")),
                Some(&metadata)
            ),
            Err(MaterializeError::UnresolvedDynamicSocket { .. })
        ));
        // A dynamic socket grant with no host-socket field is a misconfiguration.
        assert!(matches!(
            check_dynamic_socket(
                &dynamic_socket_spec(Some("unix:///run/omnifs/k8s.sock")),
                None
            ),
            Err(MaterializeError::UnresolvedDynamicSocket { .. })
        ));
    }
}

#[cfg(test)]
mod dynamic_domain_tests {
    use super::*;
    use crate::ids::{ProviderId, ProviderMeta, ProviderName, ProviderRef};

    fn dynamic_domain_spec(domains: &serde_json::Value) -> Spec {
        let provider = ProviderRef {
            id: ProviderId::from_wasm_bytes(b"web"),
            meta: ProviderMeta {
                name: ProviderName::new("web").unwrap(),
                version: None,
            },
        };
        serde_json::from_value(serde_json::json!({
            "provider": provider,
            "mount": "web",
            "capabilities": { "domains": { "dynamic": true } },
            "config": { "domains": domains },
        }))
        .expect("spec parses")
    }

    fn domain_metadata() -> ConfigMetadata {
        serde_json::from_value(serde_json::json!({
            "fields": [{
                "name": "domains",
                "type": { "kind": "array", "items": { "kind": "string" } }
            }]
        }))
        .expect("config metadata parses")
    }

    #[test]
    fn dynamic_domains_require_non_empty_bare_hostnames() {
        let metadata = domain_metadata();
        assert!(
            check_dynamic_domains(
                &dynamic_domain_spec(&serde_json::json!(["API.Example.COM"])),
                Some(&metadata)
            )
            .is_ok()
        );

        for domains in [
            serde_json::json!([]),
            serde_json::json!([""]),
            serde_json::json!(["example.com/path"]),
            serde_json::json!(["example.com:443"]),
            serde_json::json!(["*"]),
            serde_json::json!(["example..com"]),
        ] {
            assert!(
                matches!(
                    check_dynamic_domains(&dynamic_domain_spec(&domains), Some(&metadata)),
                    Err(MaterializeError::UnresolvedDynamicDomains { .. })
                ),
                "expected invalid dynamic domains to fail"
            );
        }
    }
}

/// Canonicalize every literal preopen's host path in place, so the runtime
/// (daemon or provider sandbox) can open it directly. A preopen whose host
/// already equals its guest is runtime-native: the path is provided in the
/// runtime's own filesystem (a dev fixture bind such as the db provider's
/// `/data`), not by materialization, so it passes through unrewritten.
fn canonicalize_preopens(spec: &mut Spec) -> Result<(), MaterializeError> {
    let Some(Grant::Literal(preopens)) = spec
        .capabilities
        .as_mut()
        .and_then(|capabilities| capabilities.preopened_paths.as_mut())
    else {
        return Ok(());
    };

    for preopen in preopens.iter_mut() {
        if preopen.host == preopen.guest {
            continue;
        }
        let host_path = Path::new(&preopen.host).canonicalize().map_err(|source| {
            MaterializeError::PreopenPath {
                path: preopen.host.clone(),
                source,
            }
        })?;
        if !host_path.is_dir() {
            return Err(MaterializeError::PreopenNotDir(
                host_path.display().to_string(),
            ));
        }
        preopen.host = host_path.display().to_string();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mounts::Spec;

    /// A valid-but-unresolvable 64-hex provider id. These tests exercise preopen
    /// rewriting and runtime-capability grants, none of which resolve the
    /// artifact, so the spec only needs to parse.
    const DUMMY_PROVIDER_ID: &str =
        "0000000000000000000000000000000000000000000000000000000000000000";

    /// Build a `Spec` from a JSON `body` (no `provider` field) plus a pinned
    /// `ProviderRef` named `name` with a dummy id.
    fn spec_with_provider(name: &str, body: &str) -> Spec {
        let mut value: serde_json::Value = serde_json::from_str(body).unwrap();
        value["provider"] =
            serde_json::json!({ "id": DUMMY_PROVIDER_ID, "meta": { "name": name } });
        serde_json::from_value(value).unwrap()
    }

    /// A catalog over an empty providers dir. `pinned_manifest` finds no
    /// retained artifact and returns `Ok(None)`, so the capability checks are
    /// skipped and the spec passes through to preopen rewriting unchanged.
    fn builtin_catalog(root: &std::path::Path) -> Catalog {
        Catalog::open(root.join("providers"))
    }

    #[test]
    fn canonicalizes_host_preopen_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let db_dir = tmp.path().join("db");
        std::fs::create_dir_all(&db_dir).unwrap();
        let canonical = db_dir.canonicalize().unwrap();
        let spec = spec_with_provider(
            "db",
            &format!(
                r#"{{
                "mount": "db",
                "config": {{"path": "/data/chinook.sqlite"}},
                "capabilities": {{
                    "preopened_paths": [{{"host": "{}", "guest": "/data", "mode": "ro"}}]
                }}
            }}"#,
                db_dir.display()
            ),
        );

        let out = materialize(spec, &builtin_catalog(tmp.path())).unwrap();

        let preopen = &out
            .capabilities
            .as_ref()
            .unwrap()
            .preopened_paths
            .as_ref()
            .unwrap()
            .literal()[0];
        assert_eq!(preopen.host, canonical.display().to_string());
        assert_eq!(preopen.guest, "/data");
    }

    /// A runtime-native preopen (host == guest) is provided by a fixture bind in
    /// the runtime's own filesystem, so it must pass through without being
    /// canonicalized against the materializing host (where `/data` need not
    /// exist).
    #[test]
    fn runtime_native_preopen_passes_through() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = spec_with_provider(
            "db",
            r#"{
            "mount": "db",
            "config": {"path": "/data/test.db"},
            "capabilities": {
                "preopened_paths": [{"host": "/data", "guest": "/data", "mode": "ro"}]
            }
        }"#,
        );

        let out = materialize(spec, &builtin_catalog(tmp.path())).unwrap();

        let preopen = &out
            .capabilities
            .as_ref()
            .unwrap()
            .preopened_paths
            .as_ref()
            .unwrap()
            .literal()[0];
        assert_eq!(preopen.host, "/data");
        assert_eq!(preopen.guest, "/data");
    }
}
