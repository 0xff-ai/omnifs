//! Durable authority for token-authenticated namespace listeners.

use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::io::write_atomic;

const STORE_VERSION: u32 = 1;

/// One token-authenticated namespace listener that a frontend can reconnect to.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "lowercase", deny_unknown_fields)]
pub enum Target {
    Tcp { addr: SocketAddr, token: String },
    Vsock { socket_path: PathBuf, token: String },
}

impl Target {
    fn transport(&self) -> Transport {
        match self {
            Self::Tcp { .. } => Transport::Tcp,
            Self::Vsock { .. } => Transport::Vsock,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transport {
    Tcp,
    Vsock,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    version: u32,
    targets: Vec<Target>,
}

/// Synchronized durable storage for dynamic namespace attach authority.
pub struct Store {
    path: PathBuf,
    targets: Mutex<Vec<Target>>,
}

impl Store {
    /// Open the current target file, treating a missing file as empty.
    pub fn open(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        let targets = match std::fs::read(&path) {
            Ok(bytes) => {
                let file: File = serde_json::from_slice(&bytes).map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("parse attach targets {}: {error}", path.display()),
                    )
                })?;
                if file.version != STORE_VERSION {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "attach targets at {} have version {}; this build understands only version {}",
                            path.display(),
                            file.version,
                            STORE_VERSION
                        ),
                    ));
                }
                let targets = sorted(file.targets);
                if targets
                    .windows(2)
                    .any(|pair| pair[0].transport() == pair[1].transport())
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "attach targets at {} contain duplicate transports",
                            path.display()
                        ),
                    ));
                }
                targets
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error),
        };
        Ok(Self {
            path,
            targets: Mutex::new(targets),
        })
    }

    /// Return the stable transport-ordered target snapshot.
    pub fn targets(&self) -> Vec<Target> {
        self.targets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Replace the current target for the same transport and persist it.
    pub fn set(&self, target: Target) -> io::Result<()> {
        let mut guard = self
            .targets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = guard.clone();
        guard.retain(|existing| existing.transport() != target.transport());
        guard.push(target);
        guard.sort();
        if let Err(error) = self.write(&guard) {
            *guard = previous;
            return Err(error);
        }
        Ok(())
    }

    /// Remove one exact target and persist the result.
    pub fn remove(&self, target: &Target) -> io::Result<()> {
        let mut guard = self
            .targets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = guard.clone();
        guard.retain(|existing| existing != target);
        if *guard == previous {
            return Ok(());
        }
        if let Err(error) = self.write(&guard) {
            *guard = previous;
            return Err(error);
        }
        Ok(())
    }

    fn write(&self, targets: &[Target]) -> io::Result<()> {
        let file = File {
            version: STORE_VERSION,
            targets: targets.to_vec(),
        };
        let bytes = serde_json::to_vec_pretty(&file)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        write_atomic(&self.path, &bytes, 0o600)
    }
}

fn sorted(mut targets: Vec<Target>) -> Vec<Target> {
    targets.sort();
    targets
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tcp(port: u16, token: char) -> Target {
        Target::Tcp {
            addr: SocketAddr::from(([127, 0, 0, 1], port)),
            token: token.to_string().repeat(32),
        }
    }

    fn vsock(path: &str, token: char) -> Target {
        Target::Vsock {
            socket_path: PathBuf::from(path),
            token: token.to_string().repeat(32),
        }
    }

    #[test]
    fn missing_file_is_empty_and_set_is_sorted_by_transport() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("targets.json");
        let store = Store::open(&path).unwrap();
        assert!(store.targets().is_empty());

        store.set(vsock("/vsock.sock", 'b')).unwrap();
        store.set(tcp(2, 'a')).unwrap();
        assert_eq!(
            store.targets(),
            vec![tcp(2, 'a'), vsock("/vsock.sock", 'b')]
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap()).unwrap()["version"],
            STORE_VERSION
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn set_replaces_only_the_same_transport_and_remove_is_exact() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("targets.json")).unwrap();
        let first = tcp(1, 'a');
        let replacement = tcp(2, 'b');
        let other = vsock("/vsock.sock", 'c');
        store.set(first.clone()).unwrap();
        store.set(other.clone()).unwrap();
        store.set(replacement.clone()).unwrap();
        assert_eq!(store.targets(), vec![replacement.clone(), other.clone()]);
        store.remove(&first).unwrap();
        assert_eq!(store.targets(), vec![replacement.clone(), other.clone()]);
        store.remove(&replacement).unwrap();
        assert_eq!(store.targets(), vec![other]);
    }

    #[test]
    fn strict_version_and_unknown_fields_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("targets.json");
        std::fs::write(&path, br#"{"version":99,"targets":[]}"#).unwrap();
        assert!(Store::open(&path).is_err());
        std::fs::write(&path, br#"{"version":1,"targets":[],"obsolete":true}"#).unwrap();
        assert!(Store::open(&path).is_err());
        std::fs::write(
            &path,
            br#"{"version":1,"targets":[{"transport":"tcp","addr":"bad","token":"x"}]}"#,
        )
        .unwrap();
        assert!(Store::open(&path).is_err());
        std::fs::write(
            &path,
            br#"{"version":1,"targets":[{"transport":"tcp","addr":"127.0.0.1:1","token":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},{"transport":"tcp","addr":"127.0.0.1:2","token":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}]}"#,
        )
        .unwrap();
        assert!(Store::open(&path).is_err());
    }

    #[test]
    fn failed_persistence_rolls_back_memory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("targets.json");
        let store = Store::open(&path).unwrap();
        let target = tcp(1, 'a');
        std::fs::create_dir(&path).unwrap();
        assert!(store.set(target).is_err());
        assert!(store.targets().is_empty());
    }
}
