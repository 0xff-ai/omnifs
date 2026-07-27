//! Strict persisted configuration for named filesystem instances.

use fs2::FileExt as _;
use omnifs_core::fs;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

const LOCKS_DIR: &str = ".locks";

#[derive(Debug, Clone)]
pub struct Registry {
    root: PathBuf,
}

impl Registry {
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn get(&self, id: &fs::Id) -> Result<Option<fs::Spec>, Error> {
        Self::read_path(id, &self.spec_path(id))
    }

    pub fn list(&self) -> Result<Vec<fs::Spec>, Error> {
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

    pub fn claim(&self, id: &fs::Id) -> Result<Claim<'_>, Error> {
        crate::io::ensure_private_dir(&self.root).map_err(|source| Error::Io {
            path: self.root.clone(),
            source,
        })?;
        let locks = self.root.join(LOCKS_DIR);
        crate::io::ensure_private_dir(&locks).map_err(|source| Error::Io {
            path: locks.clone(),
            source,
        })?;
        let path = locks.join(format!("{id}.lock"));
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let file = options.open(&path).map_err(|source| Error::Io {
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

pub struct Claim<'a> {
    registry: &'a Registry,
    id: fs::Id,
    _lock: File,
}

impl Claim<'_> {
    pub fn get(&self) -> Result<Option<fs::Spec>, Error> {
        self.registry.get(&self.id)
    }

    pub fn create(&self, spec: &fs::Spec) -> Result<(), Error> {
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
        crate::io::write_atomic(&path, &bytes, 0o600).map_err(|source| Error::Io { path, source })
    }

    pub fn remove(&self) -> Result<(), Error> {
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
pub enum Error {
    #[error("scan filesystem specs at {}: {source}", path.display())]
    Scan { path: PathBuf, source: io::Error },
    #[error("read or write filesystem state at {}: {source}", path.display())]
    Io { path: PathBuf, source: io::Error },
    #[error("parse filesystem spec at {}: {source}", path.display())]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("serialize filesystem spec at {}: {source}", path.display())]
    Serialize {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("invalid filesystem spec filename {}", path.display())]
    InvalidFilename { path: PathBuf },
    #[error("invalid filesystem spec name in {}: {source}", path.display())]
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

    fn spec(id: &str) -> fs::Spec {
        fs::Spec::new(
            fs::Id::new(id).unwrap(),
            fs::Protocol::Fuse,
            fs::Runtime::Docker,
            PathBuf::from(fs::GUEST_LOCATION),
        )
        .unwrap()
    }

    #[test]
    fn create_is_strict_atomic_and_duplicate_rejecting() {
        let temp = tempfile::tempdir().unwrap();
        let registry = Registry::new(temp.path().join("filesystems"));
        let id = fs::Id::new("main").unwrap();
        let claim = registry.claim(&id).unwrap();
        claim.create(&spec("main")).unwrap();
        assert!(matches!(
            claim.create(&spec("main")),
            Err(Error::AlreadyExists(_))
        ));
        assert_eq!(claim.get().unwrap().unwrap(), spec("main"));
        drop(claim);

        assert_eq!(registry.list().unwrap(), vec![spec("main")]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(registry.spec_path(&id))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn strict_read_rejects_unknown_fields_and_filename_mismatch() {
        let temp = tempfile::tempdir().unwrap();
        let registry = Registry::new(temp.path().join("filesystems"));
        std::fs::create_dir_all(&registry.root).unwrap();
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
