//! Mount materialization: turn an authored `Spec` into a runtime-ready one.
//!
//! Shared by the CLI (to compute Docker preopen binds before `docker create`)
//! and the daemon (to reconcile `mounts/*.json` into the registry). The steps,
//! in order: apply provider metadata (auth scheme and config defaults) into any
//! field the user left unset, check that the spec's capability grants satisfy
//! the manifest's declared needs, then rewrite preopens. A preopen whose host
//! equals its guest is container-native (provided in the runtime's own
//! filesystem, e.g. a dev fixture bind) and passes through untouched. Otherwise,
//! on the host the preopen host path is canonicalized in place and no binds are
//! emitted; for a container each preopen is rewritten to a stable guest path
//! under [`GUEST_PREOPENS_DIR`] and the corresponding host bind is returned for
//! the launcher to pass to `docker create`.

use std::path::{Path, PathBuf};

use omnifs_caps::{Grant, PreopenMode};
use omnifs_provider::{ConfigSchema, HostResourceKind};

use crate::mounts::{Catalog, Error as MountError, Spec};

/// Guest directory each container preopen is rewritten under, as
/// `<GUEST_PREOPENS_DIR>/<mount>/<index>`.
pub const GUEST_PREOPENS_DIR: &str = "/run/omnifs/preopens";

/// A host directory to bind into the container, paired with the guest path the
/// provider sandbox will preopen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreopenBind {
    pub host: PathBuf,
    pub container: String,
    pub mode: PreopenMode,
}

impl PreopenBind {
    #[must_use]
    pub fn docker_bind_spec(&self) -> String {
        let mode = match self.mode {
            PreopenMode::Ro => "ro",
            PreopenMode::Rw => "rw",
        };
        format!("{}:{}:{}", self.host.display(), self.container, mode)
    }
}

/// Container bind mounts derived from user-authored preopens.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContainerPreopenBinds {
    binds: Vec<PreopenBind>,
}

impl ContainerPreopenBinds {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.binds.is_empty()
    }

    #[must_use]
    pub fn as_slice(&self) -> &[PreopenBind] {
        &self.binds
    }

    #[must_use]
    pub fn into_docker_bind_specs(self) -> Vec<String> {
        self.binds
            .into_iter()
            .map(|bind| bind.docker_bind_spec())
            .collect()
    }

    fn push(&mut self, bind: PreopenBind) {
        self.binds.push(bind);
    }
}

/// A materialized mount: the runtime-ready spec plus any container preopen
/// binds required before Docker creates the daemon container.
#[derive(Debug, Clone)]
pub struct MaterializedMount {
    spec: Spec,
    preopen_binds: ContainerPreopenBinds,
}

impl MaterializedMount {
    #[must_use]
    pub fn spec(&self) -> &Spec {
        &self.spec
    }

    #[must_use]
    pub fn into_spec(self) -> Spec {
        self.spec
    }

    #[must_use]
    pub fn preopen_binds(&self) -> &ContainerPreopenBinds {
        &self.preopen_binds
    }

    #[must_use]
    pub fn into_preopen_binds(self) -> ContainerPreopenBinds {
        self.preopen_binds
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaterializationMode {
    HostNative,
    Docker,
}

impl MaterializationMode {
    fn opens_host_paths(self) -> bool {
        matches!(self, Self::HostNative)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MaterializeError {
    #[error("apply provider metadata: {0}")]
    Metadata(#[source] MountError),
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
}

/// Materialize `spec` against `catalog`.
///
/// In [`MaterializationMode::HostNative`] the daemon opens preopen directories
/// directly, so host paths are canonicalized in place and no binds are
/// returned. In [`MaterializationMode::Docker`] each user preopen is rewritten
/// to a container path and its host bind is collected for the launcher.
pub fn materialize(
    mut spec: Spec,
    catalog: &Catalog,
    mode: MaterializationMode,
) -> Result<MaterializedMount, MaterializeError> {
    // Required-capabilities check: the spec's grants must satisfy every
    // capability the pinned manifest declares the provider needs, so an
    // under-granted mount fails here rather than at the provider's first denied
    // callout. Over-granting beyond the manifest is allowed; the over-grant
    // check is deliberately not enforced (docs/future/provider-contract-versioning.md).
    if let Some(applied) = catalog
        .apply_metadata_and_needs(&mut spec)
        .map_err(MaterializeError::Metadata)?
    {
        let missing = spec
            .capabilities
            .clone()
            .unwrap_or_default()
            .satisfies(&applied.needs);
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
        check_dynamic_socket(&spec, applied.config_schema.as_ref())?;
    }

    let preopen_binds = rewrite_preopens(&mut spec, mode)?;

    Ok(MaterializedMount {
        spec,
        preopen_binds,
    })
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
    schema: Option<&ConfigSchema>,
) -> Result<(), MaterializeError> {
    let is_dynamic = spec
        .capabilities
        .as_ref()
        .and_then(|caps| caps.unix_sockets.as_ref())
        .is_some_and(|grant| matches!(grant, Grant::Dynamic(_)));
    if !is_dynamic {
        return Ok(());
    }
    let Some(field) = schema.and_then(|schema| schema.resource_field(HostResourceKind::Socket))
    else {
        return Err(MaterializeError::UnresolvedDynamicSocket {
            mount: spec.mount.clone(),
            detail: "no config field is marked as a host socket".to_string(),
        });
    };
    let endpoint = spec
        .config_raw
        .as_ref()
        .and_then(|config| config.as_value().get(field))
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

#[cfg(test)]
mod dynamic_socket_tests {
    use super::*;
    use omnifs_core::{ProviderId, ProviderMeta, ProviderName, ProviderRef};

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

    fn socket_schema() -> ConfigSchema {
        serde_json::from_value(serde_json::json!({
            "type": "object",
            "properties": {
                "endpoint": { "type": "string", "x-omnifs-resource": { "kind": "socket" } }
            }
        }))
        .expect("schema parses")
    }

    #[test]
    fn dynamic_socket_validation_errors() {
        let schema = socket_schema();
        assert!(
            check_dynamic_socket(
                &dynamic_socket_spec(Some("unix:///run/omnifs/k8s.sock")),
                Some(&schema)
            )
            .is_ok()
        );
        assert!(matches!(
            check_dynamic_socket(&dynamic_socket_spec(None), Some(&schema)),
            Err(MaterializeError::UnresolvedDynamicSocket { .. })
        ));
        assert!(matches!(
            check_dynamic_socket(
                &dynamic_socket_spec(Some("https://example.com")),
                Some(&schema)
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

fn rewrite_preopens(
    spec: &mut Spec,
    mode: MaterializationMode,
) -> Result<ContainerPreopenBinds, MaterializeError> {
    let mount = spec.mount.clone();
    let Some(Grant::Literal(preopens)) = spec
        .capabilities
        .as_mut()
        .and_then(|capabilities| capabilities.preopened_paths.as_mut())
    else {
        return Ok(ContainerPreopenBinds::default());
    };

    let mut binds = ContainerPreopenBinds::default();
    for (index, preopen) in preopens.iter_mut().enumerate() {
        // A preopen whose host already equals its guest is container-native: the
        // path is provided in the runtime's own filesystem (a dev fixture bind
        // such as the db provider's `/data`, or the daemon's host in
        // host-native mode), not by the launcher. Materialization runs both on
        // the host (to compute Docker binds) and inside the container (the
        // daemon reconciling `mounts/`); the host has no such path, so neither
        // canonicalize it nor emit a bind. The runtime opens it directly.
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
        if mode.opens_host_paths() {
            // The daemon opens the real host directory directly through
            // wasmtime, so the spec keeps the canonical host path.
            preopen.host = host_path.display().to_string();
            continue;
        }
        let container = format!("{GUEST_PREOPENS_DIR}/{mount}/{index}");
        preopen.host.clone_from(&container);
        binds.push(PreopenBind {
            host: host_path,
            container,
            mode: preopen.mode,
        });
    }
    Ok(binds)
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

    /// A catalog over empty dirs. `apply_metadata` finds no retained artifact and
    /// returns `Ok(false)`, leaving the user-authored spec fields as-is.
    fn builtin_catalog(root: &std::path::Path) -> Catalog {
        Catalog::new(root.join("mounts"), root.join("providers"))
    }

    #[test]
    fn container_rewrites_user_preopen_and_emits_bind() {
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

        let out = materialize(
            spec,
            &builtin_catalog(tmp.path()),
            MaterializationMode::Docker,
        )
        .unwrap();

        assert_eq!(
            out.preopen_binds().as_slice(),
            &[PreopenBind {
                host: canonical,
                container: format!("{GUEST_PREOPENS_DIR}/db/0"),
                mode: PreopenMode::Ro,
            }]
        );
        let preopen = &out
            .spec()
            .capabilities
            .as_ref()
            .unwrap()
            .preopened_paths
            .as_ref()
            .unwrap()
            .literal()[0];
        assert_eq!(preopen.host, format!("{GUEST_PREOPENS_DIR}/db/0"));
        assert_eq!(preopen.guest, "/data");
    }

    #[test]
    fn host_native_keeps_canonical_host_and_emits_no_bind() {
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

        let out = materialize(
            spec,
            &builtin_catalog(tmp.path()),
            MaterializationMode::HostNative,
        )
        .unwrap();

        assert!(out.preopen_binds().is_empty());
        let preopen = &out
            .spec()
            .capabilities
            .as_ref()
            .unwrap()
            .preopened_paths
            .as_ref()
            .unwrap()
            .literal()[0];
        assert_eq!(preopen.host, canonical.display().to_string());
    }

    /// A container-native preopen (host == guest) is provided by a fixture bind
    /// in the runtime's own filesystem, so it must pass through without being
    /// canonicalized against the materializing host (where `/data` need not
    /// exist) and without emitting a launcher bind. Holds in both modes.
    #[test]
    fn container_native_preopen_passes_through() {
        for mode in [MaterializationMode::Docker, MaterializationMode::HostNative] {
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

            let out = materialize(spec, &builtin_catalog(tmp.path()), mode).unwrap();

            assert!(out.preopen_binds().is_empty(), "mode {mode:?}");
            let preopen = &out
                .spec()
                .capabilities
                .as_ref()
                .unwrap()
                .preopened_paths
                .as_ref()
                .unwrap()
                .literal()[0];
            assert_eq!(preopen.host, "/data", "mode {mode:?}");
            assert_eq!(preopen.guest, "/data", "mode {mode:?}");
        }
    }
}
