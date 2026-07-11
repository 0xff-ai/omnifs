use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[cfg(unix)]
const STATE_FILE_MODE: u32 = 0o600;

#[derive(Debug, Error)]
pub enum StateError {
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("{0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NfsMountState {
    pub version: u8,
    pub mount_point: PathBuf,
    pub addr: String,
    pub pid: u32,
}

impl NfsMountState {
    pub const VERSION: u8 = 1;

    fn current(mount_point: &Path, addr: SocketAddr) -> Self {
        Self {
            version: Self::VERSION,
            mount_point: mount_point.to_path_buf(),
            addr: addr.to_string(),
            pid: std::process::id(),
        }
    }

    pub fn read_all(dir: &Path) -> Result<Vec<Self>, StateError> {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };

        let mut paths = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
            })
            .collect::<Vec<_>>();
        paths.sort();

        Ok(paths
            .into_iter()
            .filter_map(|path| Self::read_file(&path).ok())
            .filter(|state| state.version == Self::VERSION)
            .collect())
    }

    pub fn read_file(path: &Path) -> Result<Self, StateError> {
        let file = std::fs::File::open(path)?;
        serde_json::from_reader(file).map_err(Into::into)
    }
}

#[derive(Debug)]
pub struct StateFile {
    path: PathBuf,
}

impl StateFile {
    pub fn write(
        mount_point: &Path,
        addr: SocketAddr,
        state_dir: &Path,
    ) -> Result<Self, StateError> {
        let name = format!("mount-{}-{}.json", std::process::id(), addr.port());
        let path = state_dir.join(name);
        let mut file_options = OpenOptions::new();
        file_options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            file_options.mode(STATE_FILE_MODE);
        }
        let mut file = file_options.open(&path)?;
        let state = NfsMountState::current(mount_point, addr);
        serde_json::to_writer_pretty(&mut file, &state)?;
        writeln!(file)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(STATE_FILE_MODE))?;
        }
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for StateFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::{NfsMountState, StateFile};
    use serde_json::Value;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::path::{Path, PathBuf};

    fn temp_state_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "omnifs-mtab-state-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp state dir");
        dir
    }

    #[test]
    fn state_file_is_json_and_removed_on_drop() {
        let dir = temp_state_dir();
        let guard = StateFile::write(
            Path::new("/mnt/omnifs"),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 2049),
            &dir,
        )
        .expect("state file");
        let path = guard.path().to_path_buf();
        let state: Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read state")).expect("json");

        assert_eq!(state["version"], 1);
        assert_eq!(state["mount_point"], "/mnt/omnifs");
        assert_eq!(state["addr"], "127.0.0.1:2049");
        assert!(state["pid"].as_u64().is_some());
        let states = NfsMountState::read_all(&dir).expect("mount states");
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].mount_point, PathBuf::from("/mnt/omnifs"));

        drop(guard);
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
