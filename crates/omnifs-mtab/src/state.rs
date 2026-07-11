use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[cfg(unix)]
const STATE_DIR_MODE: u32 = 0o700;
#[cfg(unix)]
const STATE_FILE_MODE: u32 = 0o600;

#[derive(Debug, Error)]
pub enum StateError {
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("{0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported mount state version {0}")]
    UnsupportedVersion(u64),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum MountKind {
    Fuse,
    Nfs { addr: SocketAddr },
}

impl MountKind {
    pub fn nfs_addr(&self) -> Option<SocketAddr> {
        match self {
            Self::Nfs { addr } => Some(*addr),
            Self::Fuse => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MountState {
    pub version: u8,
    pub mount_point: PathBuf,
    pub pid: u32,
    #[serde(flatten)]
    pub kind: MountKind,
}

impl MountState {
    pub const VERSION: u8 = 2;

    fn current(mount_point: &Path, kind: MountKind) -> Self {
        Self {
            version: Self::VERSION,
            mount_point: mount_point.to_path_buf(),
            pid: std::process::id(),
            kind,
        }
    }

    pub fn read_all(dir: &Path) -> Result<Vec<Self>, StateError> {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };

        let mut paths = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if Self::is_file(&path) {
                paths.push(path);
            }
        }
        paths.sort();

        paths
            .into_iter()
            .map(|path| Self::read_file(&path))
            .collect()
    }

    pub fn read_file(path: &Path) -> Result<Self, StateError> {
        let file = std::fs::File::open(path)?;
        let value: serde_json::Value = serde_json::from_reader(file)?;
        if value.get("version").and_then(serde_json::Value::as_u64)
            != Some(u64::from(Self::VERSION))
        {
            return Err(StateError::UnsupportedVersion(
                value
                    .get("version")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or_default(),
            ));
        }
        serde_json::from_value(value).map_err(Into::into)
    }

    pub fn is_file(path: &Path) -> bool {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name.starts_with("mount-")
                    && Path::new(name)
                        .extension()
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
            })
    }
}

#[derive(Debug)]
pub struct StateFile {
    path: PathBuf,
}

impl StateFile {
    pub fn write_fuse(mount_point: &Path, state_dir: &Path) -> Result<Self, StateError> {
        Self::write(
            &MountState::current(mount_point, MountKind::Fuse),
            state_dir,
            None,
        )
    }

    pub fn write_nfs(
        mount_point: &Path,
        addr: SocketAddr,
        state_dir: &Path,
    ) -> Result<Self, StateError> {
        Self::write(
            &MountState::current(mount_point, MountKind::Nfs { addr }),
            state_dir,
            Some(addr.port()),
        )
    }

    fn write(
        state: &MountState,
        state_dir: &Path,
        discriminator: Option<u16>,
    ) -> Result<Self, StateError> {
        ensure_private_state_dir(state_dir)?;
        let name = match discriminator {
            Some(discriminator) => format!("mount-{}-{discriminator}.json", state.pid),
            None => format!("mount-{}.json", state.pid),
        };
        let path = state_dir.join(name);
        let mut file_options = OpenOptions::new();
        file_options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            file_options.mode(STATE_FILE_MODE);
        }
        let mut file = file_options.open(&path)?;
        serde_json::to_writer_pretty(&mut file, state)?;
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

fn ensure_private_state_dir(state_dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(STATE_DIR_MODE)
            .create(state_dir)?;
        std::fs::set_permissions(state_dir, std::fs::Permissions::from_mode(STATE_DIR_MODE))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(state_dir)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{MountKind, MountState, StateFile};
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
    fn nfs_state_file_is_json_and_removed_on_drop() {
        let dir = temp_state_dir();
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 2049);
        let guard = StateFile::write_nfs(Path::new("/mnt/omnifs"), addr, &dir).expect("state file");
        let path = guard.path().to_path_buf();
        let state: Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read state")).expect("json");

        assert_eq!(state["version"], 2);
        assert_eq!(state["kind"], "nfs");
        assert_eq!(state["mount_point"], "/mnt/omnifs");
        assert_eq!(state["addr"], "127.0.0.1:2049");
        assert!(state["pid"].as_u64().is_some());
        let states = MountState::read_all(&dir).expect("mount states");
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].mount_point, PathBuf::from("/mnt/omnifs"));
        assert_eq!(states[0].kind.nfs_addr(), Some(addr));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        drop(guard);
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn fuse_state_has_no_nfs_address() {
        let dir = temp_state_dir();
        let guard = StateFile::write_fuse(Path::new("/mnt/omnifs"), &dir).expect("state file");
        let states = MountState::read_all(&dir).expect("mount states");

        assert_eq!(states.len(), 1);
        assert_eq!(states[0].kind, MountKind::Fuse);
        assert_eq!(states[0].kind.nfs_addr(), None);

        drop(guard);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn read_all_ignores_non_mount_json() {
        let dir = temp_state_dir();
        std::fs::write(dir.join("filehandles.json"), b"{}\n").expect("write unrelated state");

        assert!(MountState::read_all(&dir).expect("mount states").is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn old_schema_fails_by_version_before_shape() {
        let dir = temp_state_dir();
        let path = dir.join("mount-old.json");
        std::fs::write(
            &path,
            br#"{"version":1,"mount_point":"/mnt/omnifs","addr":"127.0.0.1:2049","pid":1}"#,
        )
        .expect("write old state");

        assert!(matches!(
            MountState::read_all(&dir),
            Err(super::StateError::UnsupportedVersion(1))
        ));
        let _ = std::fs::remove_dir_all(dir);
    }
}
