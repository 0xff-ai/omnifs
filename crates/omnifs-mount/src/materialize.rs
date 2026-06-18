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

/// A materialized mount: the runtime-ready spec plus the container binds it
/// needs. `preopen_binds` is empty when materialized for the host.
#[derive(Debug, Clone)]
pub struct Materialized {
    pub spec: Spec,
    pub preopen_binds: Vec<PreopenBind>,
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
}

/// Materialize `spec` against `catalog`.
///
/// When `host_native` is true the daemon opens preopen directories directly, so
/// host paths are canonicalized in place and no binds are returned; otherwise
/// each user preopen is rewritten to a container path and its host bind is
/// collected for the launcher.
pub fn materialize(
    mut spec: Spec,
    catalog: &Catalog,
    host_native: bool,
) -> Result<Materialized, MaterializeError> {
    // Count user-authored preopens before metadata application, which may add
    // manifest-declared preopens that must not be rewritten to container paths.
    let user_preopen_count = spec
        .capabilities
        .as_ref()
        .and_then(|capabilities| capabilities.preopened_paths.as_ref())
        .map_or(0, Vec::len);

    catalog
        .apply_metadata(&mut spec)
        .map_err(MaterializeError::Metadata)?;
    spec.materialize_runtime_capabilities()
        .map_err(MaterializeError::Capabilities)?;
    let preopen_binds = rewrite_preopens(&mut spec, user_preopen_count, host_native)?;

    Ok(Materialized {
        spec,
        preopen_binds,
    })
}

fn rewrite_preopens(
    spec: &mut Spec,
    user_preopen_count: usize,
    host_native: bool,
) -> Result<Vec<PreopenBind>, MaterializeError> {
    if user_preopen_count == 0 {
        return Ok(Vec::new());
    }
    let mount = spec.mount.clone();
    let Some(preopens) = spec
        .capabilities
        .as_mut()
        .and_then(|capabilities| capabilities.preopened_paths.as_mut())
    else {
        return Ok(Vec::new());
    };

    let mut binds = Vec::new();
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
        if host_native {
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

        let out = materialize(spec, &builtin_catalog(tmp.path()), false).unwrap();

        assert_eq!(
            out.preopen_binds,
            vec![PreopenBind {
                host: canonical,
                container: format!("{GUEST_PREOPENS_DIR}/db/0"),
                mode: PreopenMode::Ro,
            }]
        );
        let preopen = &out.spec.capabilities.unwrap().preopened_paths.unwrap()[0];
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

        let out = materialize(spec, &builtin_catalog(tmp.path()), true).unwrap();

        assert!(out.preopen_binds.is_empty());
        let preopen = &out.spec.capabilities.unwrap().preopened_paths.unwrap()[0];
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

        let out = materialize(spec, &builtin_catalog(tmp.path()), false).unwrap();

        assert_eq!(
            out.spec.capabilities.unwrap().unix_sockets,
            Some(vec!["/var/run/docker.sock".to_string()])
        );
        assert!(out.preopen_binds.is_empty());
    }
}
