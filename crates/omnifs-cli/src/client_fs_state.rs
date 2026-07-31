#![allow(
    dead_code,
    reason = "Plan 006 removes the superseded client filesystem registry after the attachment cutover"
)]

//! CLI-owned filesystem configuration and runner state.
//!
//! Filesystem specs describe client-launched runtimes. They are deliberately
//! not part of daemon state: the daemon only knows live attach
//! identities received over the VFS protocol.

use fs2::FileExt as _;
use omnifs_core::fs;
use serde::Deserialize;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

const FILESYSTEMS_DIR: &str = "filesystems";
const SPECS_DIR: &str = "specs";
const LOCKS_DIR: &str = ".locks";
const STATE_DIR: &str = "state";
const RUNTIME_DIR: &str = "runtime";
const MOUNTS_DIR: &str = "mounts";
const CACHE_DIR: &str = "cache";
const GUEST_IMAGES_DIR: &str = "guest-images";

/// All durable filesystem configuration and launch artifacts owned by one CLI.
#[derive(Debug, Clone)]
pub(crate) struct ClientFilesystemState {
    root: PathBuf,
    cache_dir: PathBuf,
}

/// The small part of the profile config used by client-owned filesystem
/// runners.  Keep this type here so filesystem lifecycle code does not need to
/// open daemon state directly.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ClientConfig {
    pub(crate) metrics: ClientMetrics,
    pub(crate) filesystem: ClientFilesystemAssets,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ClientMetrics {
    pub(crate) enabled: bool,
}

impl Default for ClientMetrics {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ClientFilesystemAssets {
    pub(crate) docker_image: Option<String>,
    pub(crate) guest_image: Option<String>,
}

impl ClientFilesystemState {
    pub(crate) fn resolve() -> anyhow::Result<Self> {
        let state = Self::under_root(&crate::client_dir::client_root()?);
        state.prepare()?;
        Ok(state)
    }

    #[must_use]
    pub(crate) fn under_root(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            cache_dir: root.join(CACHE_DIR),
        }
    }

    #[must_use]
    pub(crate) fn profile_root(&self) -> &Path {
        self.root.parent().unwrap_or(&self.root)
    }

    pub(crate) fn config(&self) -> anyhow::Result<ClientConfig> {
        let path = self.profile_root().join("config.toml");
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(ClientConfig::default());
            },
            Err(error) => return Err(error.into()),
        };
        let text = std::str::from_utf8(&bytes)
            .map_err(|error| anyhow::anyhow!("parse config {}: {error}", path.display()))?;
        toml::from_str(text)
            .map_err(|error| anyhow::anyhow!("parse config {}: {error}", path.display()))
    }

    fn prepare(&self) -> io::Result<()> {
        crate::client_dir::ensure_private_dir(&self.root)?;
        crate::client_dir::ensure_private_dir(&self.root.join(FILESYSTEMS_DIR))?;
        crate::client_dir::ensure_private_dir(&self.cache_dir)
    }

    #[must_use]
    pub(crate) fn registry(&self) -> Registry {
        Registry::new(self.root.join(FILESYSTEMS_DIR).join(SPECS_DIR))
    }

    #[must_use]
    pub(crate) fn state_root(&self) -> PathBuf {
        self.root.join(FILESYSTEMS_DIR).join(STATE_DIR)
    }

    #[must_use]
    pub(crate) fn runtime_root(&self) -> PathBuf {
        self.root.join(FILESYSTEMS_DIR).join(RUNTIME_DIR)
    }

    #[must_use]
    pub(crate) fn guest_image_cache(&self) -> PathBuf {
        self.cache_dir.join(GUEST_IMAGES_DIR)
    }

    pub(crate) fn runtime_paths(&self) -> anyhow::Result<omnifs_fs_runtime::RuntimePaths> {
        Ok(omnifs_fs_runtime::RuntimePaths::new(
            self.profile_root().to_path_buf(),
            std::env::var_os(omnifs_bootstrap::OMNIFS_HOME_ENV).is_none(),
            self.state_root(),
            self.cache_dir.clone(),
            self.runtime_root(),
            self.guest_image_cache(),
            std::env::current_exe()
                .map_err(|error| anyhow::anyhow!("resolve the omnifs executable: {error}"))?,
        ))
    }

    #[must_use]
    pub(crate) fn default_host_location(&self, id: &fs::Id) -> PathBuf {
        self.root
            .join(FILESYSTEMS_DIR)
            .join(MOUNTS_DIR)
            .join(id.as_str())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Registry {
    root: PathBuf,
}

impl Registry {
    #[must_use]
    pub(crate) fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn get(&self, id: &fs::Id) -> Result<Option<fs::Spec>, Error> {
        Self::read_path(id, &self.spec_path(id))
    }

    pub(crate) fn list(&self) -> Result<Vec<fs::Spec>, Error> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(Error::Scan {
                    path: self.root.clone(),
                    source,
                });
            },
        };
        let mut paths = Vec::new();
        for entry in entries {
            let path = entry
                .map_err(|source| Error::Scan {
                    path: self.root.clone(),
                    source,
                })?
                .path();
            if path.extension().and_then(|value| value.to_str()) == Some("json") {
                paths.push(path);
            }
        }
        paths.sort();
        paths
            .into_iter()
            .map(|path| {
                let stem = path
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .ok_or_else(|| Error::InvalidFilename { path: path.clone() })?;
                let id = fs::Id::new(stem).map_err(|source| Error::InvalidId {
                    path: path.clone(),
                    source,
                })?;
                Self::read_path(&id, &path)?.ok_or(Error::Disappeared(path))
            })
            .collect()
    }

    pub(crate) fn claim(&self, id: &fs::Id) -> Result<Claim<'_>, Error> {
        crate::client_dir::ensure_private_dir(&self.root).map_err(|source| Error::Io {
            path: self.root.clone(),
            source,
        })?;
        let locks = self.root.join(LOCKS_DIR);
        crate::client_dir::ensure_private_dir(&locks).map_err(|source| Error::Io {
            path: locks.clone(),
            source,
        })?;
        let path = locks.join(format!("{id}.lock"));
        let file = crate::client_dir::open_private_sidecar(&path).map_err(|source| Error::Io {
            path: path.clone(),
            source,
        })?;
        file.try_lock_exclusive().map_err(|source| Error::Lock {
            id: id.clone(),
            source,
        })?;
        Ok(Claim {
            registry: self,
            id: id.clone(),
            _lock: file,
        })
    }

    fn read_path(id: &fs::Id, path: &Path) -> Result<Option<fs::Spec>, Error> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(Error::Io {
                    path: path.to_path_buf(),
                    source,
                });
            },
        };
        let spec: fs::Spec = serde_json::from_slice(&bytes).map_err(|source| Error::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        if spec.id() != id {
            return Err(Error::FilenameMismatch {
                path: path.to_path_buf(),
                declared: spec.id().clone(),
            });
        }
        Ok(Some(spec))
    }

    fn spec_path(&self, id: &fs::Id) -> PathBuf {
        self.root.join(format!("{id}.json"))
    }
}

pub(crate) struct Claim<'a> {
    registry: &'a Registry,
    id: fs::Id,
    _lock: File,
}

impl Claim<'_> {
    pub(crate) fn get(&self) -> Result<Option<fs::Spec>, Error> {
        self.registry.get(&self.id)
    }

    pub(crate) fn create(&self, spec: &fs::Spec) -> Result<(), Error> {
        if spec.id() != &self.id {
            return Err(Error::ClaimMismatch {
                claimed: self.id.clone(),
                actual: spec.id().clone(),
            });
        }
        if self.get()?.is_some() {
            return Err(Error::AlreadyExists(self.id.clone()));
        }
        let path = self.registry.spec_path(&self.id);
        let mut bytes = serde_json::to_vec_pretty(spec).map_err(|source| Error::Serialize {
            path: path.clone(),
            source,
        })?;
        bytes.push(b'\n');
        crate::client_dir::write_private_atomic(&path, &bytes)
            .map_err(|source| Error::Io { path, source })
    }

    pub(crate) fn remove(&self) -> Result<(), Error> {
        let path = self.registry.spec_path(&self.id);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                Err(Error::NotFound(self.id.clone()))
            },
            Err(source) => Err(Error::Io { path, source }),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("scan filesystem specs at {}: {source}", path.display())]
    Scan { path: PathBuf, source: io::Error },
    #[error("read or write filesystem state at {}: {source}", path.display())]
    Io { path: PathBuf, source: io::Error },
    #[error("parse filesystem spec at {}: {source}", path.display())]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("serialize filesystem spec at {path}: {source}")]
    Serialize {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("invalid filesystem spec filename {}", path.display())]
    InvalidFilename { path: PathBuf },
    #[error("invalid filesystem spec name in {path}: {source}")]
    InvalidId { path: PathBuf, source: fs::IdError },
    #[error(
        "filesystem spec {} declares name `{declared}` instead of matching its filename",
        path.display()
    )]
    FilenameMismatch { path: PathBuf, declared: fs::Id },
    #[error("filesystem spec disappeared while reading {}", .0.display())]
    Disappeared(PathBuf),
    #[error("filesystem `{0}` already exists")]
    AlreadyExists(fs::Id),
    #[error("filesystem `{0}` is not configured")]
    NotFound(fs::Id),
    #[error("filesystem `{id}` is busy in another lifecycle command")]
    Lock { id: fs::Id, source: io::Error },
    #[error("filesystem claim for `{claimed}` cannot write spec `{actual}`")]
    ClaimMismatch { claimed: fs::Id, actual: fs::Id },
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn registry_is_private_and_atomic() {
        let temp = tempfile::tempdir().unwrap();
        let state = ClientFilesystemState::under_root(&temp.path().join("client"));
        state.prepare().unwrap();
        let registry = state.registry();
        let id = fs::Id::new("main").unwrap();
        let spec = fs::Spec::new(
            id.clone(),
            fs::Protocol::Fuse,
            fs::Runtime::Docker,
            PathBuf::from(fs::GUEST_LOCATION),
        )
        .unwrap();
        let claim = registry.claim(&id).unwrap();
        claim.create(&spec).unwrap();
        assert!(matches!(claim.create(&spec), Err(Error::AlreadyExists(_))));
        drop(claim);
        assert_eq!(registry.list().unwrap(), vec![spec]);
        assert_eq!(
            std::fs::metadata(state.root.join("filesystems/specs/main.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&state.root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&state.cache_dir)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[test]
    fn strict_read_rejects_unknown_fields_and_filename_mismatch() {
        let temp = tempfile::tempdir().unwrap();
        let state = ClientFilesystemState::under_root(&temp.path().join("client"));
        state.prepare().unwrap();
        let registry = state.registry();
        crate::client_dir::ensure_private_dir(&registry.root).unwrap();

        std::fs::write(
            registry.root.join("main.json"),
            r#"{"id":"other","protocol":"fuse","runtime":"docker","location":"/omnifs"}"#,
        )
        .unwrap();
        assert!(matches!(
            registry.list(),
            Err(Error::FilenameMismatch { .. })
        ));

        std::fs::write(
            registry.root.join("main.json"),
            r#"{"id":"main","protocol":"fuse","runtime":"docker","location":"/omnifs","extra":true}"#,
        )
        .unwrap();
        assert!(matches!(registry.list(), Err(Error::Parse { .. })));
    }
}
