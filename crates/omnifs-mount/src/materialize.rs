//! Mount materialization: turn an authored `Spec` into a runtime-ready one.
//!
//! Shared by the CLI (to compute Docker preopen binds before `docker create`)
//! and the daemon (to reconcile `mounts/*.json` into the registry). The steps,
//! in order: apply provider metadata so manifest-declared capabilities are
//! present before anything reads them, grant runtime capabilities derived from
//! config (the configured unix socket), then rewrite user preopens. On the host
//! the preopen host path is canonicalized in place and no binds are emitted; for
//! a container each user preopen is rewritten to a stable guest path under
//! [`GUEST_PREOPENS_DIR`] and the corresponding host bind is returned for the
//! launcher to pass to `docker create`.

use std::path::{Path, PathBuf};

use omnifs_provider::PreopenMode;

use crate::mounts::{Catalog, Error as MountError, RuntimeCapabilitiesError, Spec};

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
    #[error("grant runtime capabilities: {0}")]
    Capabilities(#[source] RuntimeCapabilitiesError),
    #[error("canonicalize preopen `{path}`: {source}")]
    PreopenPath {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("preopen `{0}` is not a directory")]
    PreopenNotDir(String),
    /// The spec's stamped contract block does not match the live provider
    /// contract, indicating the spec was not updated through `omnifs up`
    /// after a provider upgrade. The daemon refuses to serve a mount whose
    /// contract the spec was not written against.
    #[error(
        "contract mismatch for mount `{mount}`: spec stamped against contract {spec_hash}, \
         live provider contract is {live_hash}; run `omnifs up` to reconcile"
    )]
    ContractMismatch {
        mount: String,
        spec_hash: String,
        live_hash: String,
    },
}

/// Materialize `spec` against `catalog`.
///
/// In [`MaterializationMode::HostNative`] the daemon opens preopen directories
/// directly, so host paths are canonicalized in place and no binds are
/// returned. In [`MaterializationMode::Docker`] each user preopen is rewritten
/// to a container path and its host bind is collected for the launcher.
///
/// When the spec carries a `contract` block, the live provider contract is
/// derived and compared against it. A mismatch is a hard error: the daemon
/// backstop guarantees it never serves a mount whose contract the spec was not
/// written against. The CLI clears mismatches before reconcile through the
/// `omnifs up` pre-flight; a mismatch here means the spec drifted behind the
/// CLI's back (for example by a hand-edit or an out-of-band provider swap).
pub fn materialize(
    mut spec: Spec,
    catalog: &Catalog,
    mode: MaterializationMode,
) -> Result<MaterializedMount, MaterializeError> {
    // Count user-authored preopens before metadata application, which may add
    // manifest-declared preopens that must not be rewritten to container paths.
    let user_preopen_count = spec
        .capabilities
        .as_ref()
        .and_then(|capabilities| capabilities.preopened_paths.as_ref())
        .map_or(0, Vec::len);

    // Backstop: when the spec carries a contract block, verify it matches the
    // live provider contract before proceeding. This runs before
    // `apply_metadata` so the spec's provider field is still in its authored
    // form (not yet mutated by metadata application).
    if let Some(stamped) = &spec.contract {
        let live = catalog
            .live_contract_for(&spec)
            .map_err(MaterializeError::Metadata)?;
        if let Some(live) = live {
            let spec_hash = stamped.hash();
            let live_hash = live.hash();
            if spec_hash != live_hash {
                return Err(MaterializeError::ContractMismatch {
                    mount: spec.mount.clone(),
                    spec_hash,
                    live_hash,
                });
            }
        }
        // When no live manifest is found (unknown provider), skip the check
        // and let the rest of the pipeline decide whether to proceed or fail.
    }

    catalog
        .apply_metadata(&mut spec)
        .map_err(MaterializeError::Metadata)?;
    spec.materialize_runtime_capabilities()
        .map_err(MaterializeError::Capabilities)?;
    let preopen_binds = rewrite_preopens(&mut spec, user_preopen_count, mode)?;

    Ok(MaterializedMount {
        spec,
        preopen_binds,
    })
}

fn rewrite_preopens(
    spec: &mut Spec,
    user_preopen_count: usize,
    mode: MaterializationMode,
) -> Result<ContainerPreopenBinds, MaterializeError> {
    if user_preopen_count == 0 {
        return Ok(ContainerPreopenBinds::default());
    }
    let mount = spec.mount.clone();
    let Some(preopens) = spec
        .capabilities
        .as_mut()
        .and_then(|capabilities| capabilities.preopened_paths.as_mut())
    else {
        return Ok(ContainerPreopenBinds::default());
    };

    let mut binds = ContainerPreopenBinds::default();
    for (index, preopen) in preopens.iter_mut().take(user_preopen_count).enumerate() {
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

    /// Build a catalog over empty dirs so `apply_metadata` falls back to the
    /// built-in provider manifests (db, docker, ...).
    fn builtin_catalog(root: &std::path::Path) -> Catalog {
        Catalog::new(root.join("mounts"), root.join("providers"))
    }

    #[test]
    fn container_rewrites_user_preopen_and_emits_bind() {
        let tmp = tempfile::tempdir().unwrap();
        let db_dir = tmp.path().join("db");
        std::fs::create_dir_all(&db_dir).unwrap();
        let canonical = db_dir.canonicalize().unwrap();
        let spec = Spec::parse(&format!(
            r#"{{
                "provider": "omnifs_provider_db.wasm",
                "mount": "db",
                "config": {{"database_type": "sqlite", "path": "/data/chinook.sqlite"}},
                "capabilities": {{
                    "preopened_paths": [{{"host": "{}", "guest": "/data", "mode": "ro"}}]
                }}
            }}"#,
            db_dir.display()
        ))
        .unwrap();

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
            .unwrap()[0];
        assert_eq!(preopen.host, format!("{GUEST_PREOPENS_DIR}/db/0"));
        assert_eq!(preopen.guest, "/data");
    }

    #[test]
    fn host_native_keeps_canonical_host_and_emits_no_bind() {
        let tmp = tempfile::tempdir().unwrap();
        let db_dir = tmp.path().join("db");
        std::fs::create_dir_all(&db_dir).unwrap();
        let canonical = db_dir.canonicalize().unwrap();
        let spec = Spec::parse(&format!(
            r#"{{
                "provider": "omnifs_provider_db.wasm",
                "mount": "db",
                "config": {{"database_type": "sqlite", "path": "/data/chinook.sqlite"}},
                "capabilities": {{
                    "preopened_paths": [{{"host": "{}", "guest": "/data", "mode": "ro"}}]
                }}
            }}"#,
            db_dir.display()
        ))
        .unwrap();

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
            .unwrap()[0];
        assert_eq!(preopen.host, canonical.display().to_string());
    }

    #[test]
    fn grants_configured_unix_socket_from_endpoint() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = Spec::parse(
            r#"{
                "provider": "omnifs_provider_docker.wasm",
                "mount": "docker",
                "config": {"endpoint": "unix:///var/run/docker.sock"}
            }"#,
        )
        .unwrap();

        let out = materialize(
            spec,
            &builtin_catalog(tmp.path()),
            MaterializationMode::Docker,
        )
        .unwrap();

        assert_eq!(
            out.spec()
                .capabilities
                .as_ref()
                .unwrap()
                .unix_sockets
                .clone(),
            Some(vec!["/var/run/docker.sock".to_string()])
        );
        assert!(out.preopen_binds().is_empty());
    }
}
