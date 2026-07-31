//! Narrow shared bootstrap state for the CLI and daemon.

use atomic_write_file::OpenOptions as AtomicOpenOptions;
use atomic_write_file::unix::OpenOptionsExt as _;
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::{FileTypeExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

const DEFAULT_HOME_SUBDIR: &str = ".omnifs";
const CONTROL_SOCKET_FILE: &str = "control.sock";
const PROCESS_IDENTITY_FILE: &str = "process.json";
const SPAWN_LOCK_FILE: &str = "spawn.lock";
const PROCESS_IDENTITY_VERSION: u32 = 1;

/// Environment variable selecting the Omnifs profile root.
pub const OMNIFS_HOME_ENV: &str = "OMNIFS_HOME";

/// The sole resolver for paths shared before the control socket is usable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    root: PathBuf,
}

impl Profile {
    fn resolve_root() -> Result<PathBuf, ResolveError> {
        std::env::var_os(OMNIFS_HOME_ENV)
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(DEFAULT_HOME_SUBDIR))
            })
            .ok_or(ResolveError)
    }

    fn with_root(root: &Path) -> Self {
        let resolved_root = if root.is_absolute() {
            root.to_path_buf()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(root)
        };
        Self {
            root: resolved_root,
        }
    }

    /// Return the resolved active profile directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn control_socket(&self) -> PathBuf {
        self.root.join(CONTROL_SOCKET_FILE)
    }

    #[must_use]
    pub fn process_identity_path(&self) -> PathBuf {
        self.root.join(PROCESS_IDENTITY_FILE)
    }

    fn spawn_lock_path(&self) -> PathBuf {
        self.root.join(SPAWN_LOCK_FILE)
    }

    fn read_process_identity_inner(&self) -> io::Result<Option<DaemonIdentity>> {
        let path = self.process_identity_path();
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        serde_json::from_slice(&bytes).map(Some).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("parse process identity {}: {error}", path.display()),
            )
        })
    }

    fn prepare_dir(&self) -> io::Result<()> {
        std::fs::create_dir_all(&self.root)?;
        std::fs::set_permissions(&self.root, std::fs::Permissions::from_mode(0o700))
    }

    fn remove_process_identity(&self) -> io::Result<()> {
        match std::fs::remove_file(self.process_identity_path()) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn acquire_spawn_lock_inner(&self) -> io::Result<SpawnLock> {
        self.prepare_dir()?;
        let path = self.spawn_lock_path();
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        file.lock_exclusive()?;
        Ok(SpawnLock { _file: file })
    }

    /// Resolve the active profile from `OMNIFS_HOME`, then `$HOME/.omnifs`.
    pub fn resolve() -> Result<Self, ResolveError> {
        Ok(Self::with_root(&Self::resolve_root()?))
    }

    /// Build a profile under an explicit root.
    #[must_use]
    pub fn under_root(root: &Path) -> Self {
        Self::with_root(root)
    }

    /// Read the persisted daemon identity for client-side diagnostics.
    pub fn read_process_identity(&self) -> io::Result<Option<DaemonIdentity>> {
        self.read_process_identity_inner()
    }

    /// Serialize CLI processes that may start or replace the daemon.
    pub fn acquire_spawn_lock(&self) -> io::Result<SpawnLock> {
        self.acquire_spawn_lock_inner()
    }

    /// Remove one exact daemon's identity and control-socket path while
    /// excluding concurrent startup. Replacement bootstrap state is never
    /// removed.
    pub fn remove_daemon_bootstrap_if(&self, expected: &DaemonIdentity) -> io::Result<bool> {
        let _spawn_lock = self.acquire_spawn_lock()?;
        let Some(current) = self.read_process_identity_inner()? else {
            return Ok(false);
        };
        if current != *expected {
            return Ok(false);
        }
        match std::fs::remove_file(self.control_socket()) {
            Ok(()) => {},
            Err(error) if error.kind() == io::ErrorKind::NotFound => {},
            Err(error) => return Err(error),
        }
        self.remove_process_identity()?;
        Ok(true)
    }

    /// Bind the fixed local control socket with fail-closed stale-path rules.
    pub fn bind_control_socket(&self) -> io::Result<UnixListener> {
        self.prepare_dir()?;
        let path = self.control_socket();
        prepare_control_path(&path)?;
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        Ok(listener)
    }

    pub fn write_process_identity(&self, identity: &DaemonIdentity) -> io::Result<()> {
        self.prepare_dir()?;
        let bytes = serde_json::to_vec(identity)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let mut options = AtomicOpenOptions::new();
        options.preserve_mode(false).mode(0o600);
        let mut file = options.open(self.process_identity_path())?;
        file.write_all(&bytes)?;
        file.commit()
    }

    /// Remove the identity and socket published by this daemon on shutdown.
    /// The full identity is checked while holding the spawn lock so a
    /// replacement daemon's files are never removed.
    pub fn remove_published_bootstrap_if(&self, expected: &DaemonIdentity) -> io::Result<bool> {
        let _spawn_lock = self.acquire_spawn_lock_inner()?;
        let Some(current) = self.read_process_identity_inner()? else {
            return Ok(false);
        };
        if current != *expected {
            return Ok(false);
        }
        match std::fs::remove_file(self.control_socket()) {
            Ok(()) => {},
            Err(error) if error.kind() == io::ErrorKind::NotFound => {},
            Err(error) => return Err(error),
        }
        self.remove_process_identity().map(|()| true)
    }
}

/// Held for the full daemon start or replacement decision.
pub struct SpawnLock {
    _file: File,
}

/// Narrow daemon process identity used only when RPC cannot answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonIdentity {
    version: u32,
    pid: u32,
    #[serde(rename = "instance_token")]
    token: String,
    executable: PathBuf,
    start_identity: Option<String>,
}

impl DaemonIdentity {
    pub fn current() -> io::Result<Self> {
        let pid = std::process::id();
        let mut token = [0_u8; 16];
        getrandom::fill(&mut token).map_err(io::Error::other)?;
        Ok(Self {
            version: PROCESS_IDENTITY_VERSION,
            pid,
            token: hex::encode(token),
            executable: std::fs::canonicalize(std::env::current_exe()?)?,
            start_identity: process_start_identity(pid),
        })
    }

    #[must_use]
    pub fn pid(&self) -> u32 {
        self.pid
    }

    #[must_use]
    pub fn instance_token(&self) -> &str {
        &self.token
    }

    #[must_use]
    pub fn executable(&self) -> &Path {
        &self.executable
    }

    /// Recheck PID reuse and executable identity before a process-level action.
    #[must_use]
    pub fn still_identifies_running_process(&self) -> bool {
        self.version == PROCESS_IDENTITY_VERSION
            && self.start_identity.is_some()
            && self.start_identity == process_start_identity(self.pid)
            && process_executable(self.pid).is_some_and(|path| path == self.executable)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("cannot resolve omnifs home: set HOME or OMNIFS_HOME")]
pub struct ResolveError;

fn prepare_control_path(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_socket() => std::fs::remove_file(path),
        Ok(_) => match UnixStream::connect(path) {
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!(
                    "another omnifs daemon is already serving this profile on {}",
                    path.display()
                ),
            )),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                ) =>
            {
                std::fs::remove_file(path)
            },
            Err(error) => Err(error),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(target_os = "linux")]
fn process_start_identity(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.get(stat.rfind(')')? + 1..)?
        .split_whitespace()
        .nth(19)
        .map(str::to_owned)
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn process_start_identity(pid: u32) -> Option<String> {
    let pid = i32::try_from(pid).ok()?;
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
    // SAFETY: `info` points to writable storage of exactly `size` bytes.
    // `proc_pidinfo` initializes that storage on the checked full-size return
    // and does not retain the pointer.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    // SAFETY: the full-size return above means `proc_pidinfo` initialized the
    // complete `proc_bsdinfo` value.
    let info = unsafe { info.assume_init() };
    Some(format!(
        "{}:{:06}",
        info.pbi_start_tvsec, info.pbi_start_tvusec
    ))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn process_start_identity(_pid: u32) -> Option<String> {
    None
}

#[cfg(target_os = "linux")]
fn process_executable(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn process_executable(pid: u32) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt as _;

    const MAX_PATH: usize = 4096;
    const MAX_PATH_U32: u32 = 4096;
    unsafe extern "C" {
        fn proc_pidpath(
            pid: libc::c_int,
            buffer: *mut libc::c_void,
            buffer_size: u32,
        ) -> libc::c_int;
    }

    let pid = i32::try_from(pid).ok()?;
    let mut buffer = [0_u8; MAX_PATH];
    // SAFETY: `buffer` is writable for exactly `MAX_PATH` bytes and stays
    // alive for the call. `proc_pidpath` does not retain the pointer.
    let len = unsafe { proc_pidpath(pid, buffer.as_mut_ptr().cast(), MAX_PATH_U32) };
    let len = usize::try_from(len).ok().filter(|len| *len > 0)?;
    let bytes = buffer
        .get(..len)?
        .strip_suffix(&[0])
        .unwrap_or(&buffer[..len]);
    std::fs::canonicalize(PathBuf::from(std::ffi::OsStr::from_bytes(bytes))).ok()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn process_executable(_pid: u32) -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_owns_only_fixed_bootstrap_paths() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = Profile::under_root(dir.path());
        assert_eq!(endpoint.control_socket(), dir.path().join("control.sock"));
        assert_eq!(
            endpoint.process_identity_path(),
            dir.path().join("process.json")
        );
        assert_eq!(endpoint.spawn_lock_path(), dir.path().join("spawn.lock"));
    }

    #[test]
    fn explicit_relative_root_is_anchored_to_the_current_directory() {
        let relative = Path::new("target").join("profile-relative-test");
        let profile = Profile::under_root(&relative);
        assert_eq!(
            profile.root(),
            std::env::current_dir().unwrap().join(relative)
        );
    }

    #[test]
    fn bootstrap_files_are_owner_only_and_a_live_socket_is_never_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let profile = Profile::under_root(dir.path());
        let _lock = profile.acquire_spawn_lock().unwrap();
        let identity = DaemonIdentity::current().unwrap();
        profile.write_process_identity(&identity).unwrap();
        let listener = profile.bind_control_socket().unwrap();

        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(profile.root()), 0o700);
        assert_eq!(mode(&profile.spawn_lock_path()), 0o600);
        assert_eq!(mode(&profile.process_identity_path()), 0o600);
        assert_eq!(mode(&profile.control_socket()), 0o600);

        let replacement = Profile::under_root(dir.path())
            .bind_control_socket()
            .unwrap_err();
        assert_eq!(replacement.kind(), io::ErrorKind::AddrInUse);
        assert!(profile.control_socket().exists());
        drop(listener);
    }

    #[test]
    fn process_identity_round_trips_and_matches_current_process() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = Profile::under_root(dir.path());
        let identity = DaemonIdentity::current().unwrap();
        let daemon = Profile::under_root(dir.path());
        daemon.write_process_identity(&identity).unwrap();
        assert_eq!(
            endpoint.read_process_identity().unwrap().as_ref(),
            Some(&identity)
        );
        assert_eq!(
            identity.start_identity.as_deref(),
            process_start_identity(identity.pid()).as_deref()
        );
        assert_eq!(
            Some(identity.executable().to_path_buf()),
            process_executable(identity.pid())
        );
        assert!(identity.still_identifies_running_process());
        endpoint.remove_process_identity().unwrap();
        assert!(endpoint.read_process_identity().unwrap().is_none());
    }

    #[test]
    fn exact_identity_removal_never_deletes_a_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = Profile::under_root(dir.path());
        let daemon = Profile::under_root(dir.path());
        let old = DaemonIdentity::current().unwrap();
        let replacement = DaemonIdentity::current().unwrap();
        assert_ne!(old, replacement);
        daemon.write_process_identity(&replacement).unwrap();
        std::fs::write(endpoint.control_socket(), b"replacement").unwrap();

        assert!(!endpoint.remove_daemon_bootstrap_if(&old).unwrap());
        assert_eq!(
            endpoint.read_process_identity().unwrap().as_ref(),
            Some(&replacement)
        );
        assert!(endpoint.control_socket().exists());
        assert!(endpoint.remove_daemon_bootstrap_if(&replacement).unwrap());
        assert!(endpoint.read_process_identity().unwrap().is_none());
        assert!(!endpoint.control_socket().exists());
    }

    #[test]
    fn stale_control_path_never_follows_a_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let endpoint = Profile::under_root(dir.path());
        let target = dir.path().join("target");
        std::fs::write(&target, b"keep").unwrap();
        symlink(&target, endpoint.control_socket()).unwrap();
        let listener = endpoint.bind_control_socket().unwrap();
        drop(listener);
        assert_eq!(std::fs::read(target).unwrap(), b"keep");
    }
}
