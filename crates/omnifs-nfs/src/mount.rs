use crate::adapter::Export;
use crate::error::NfsFrontendError;
use crate::server::start_server;
use omnifs_host::registry::ProviderRegistry;
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use tokio::runtime::Handle;

const MOUNT_WAIT_INTERVAL: Duration = Duration::from_millis(500);
const UNMOUNT_SETTLE_INTERVAL: Duration = Duration::from_millis(100);
const UNMOUNT_SETTLE_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(unix)]
const STATE_DIR_MODE: u32 = 0o700;
#[cfg(unix)]
const STATE_FILE_MODE: u32 = 0o600;

#[derive(Debug, Clone)]
pub struct NfsMountOptions {
    pub bind: SocketAddr,
    pub trace_path: Option<PathBuf>,
    pub state_dir: PathBuf,
}

impl NfsMountOptions {
    pub fn loopback(state_dir: PathBuf) -> Self {
        Self {
            bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            trace_path: None,
            state_dir,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NfsMountState {
    pub version: u8,
    pub mount_point: PathBuf,
    pub addr: String,
    pub pid: u32,
}

impl NfsMountState {
    const VERSION: u8 = 1;

    fn current(mount_point: &Path, addr: SocketAddr) -> Self {
        Self {
            version: Self::VERSION,
            mount_point: mount_point.to_path_buf(),
            addr: addr.to_string(),
            pid: std::process::id(),
        }
    }
}

pub fn mount_blocking(
    mount_point: &Path,
    registry: &Arc<ProviderRegistry>,
    rt: Handle,
    options: &NfsMountOptions,
) -> Result<(), NfsFrontendError> {
    std::fs::create_dir_all(mount_point)?;
    ensure_private_state_dir(&options.state_dir)?;
    sweep_stale_states(&options.state_dir);
    let signal_rx = ctrl_c_receiver(&rt);
    let export = Arc::new(Export::new(rt, Arc::clone(registry)));
    let server = start_server(export, options.bind, options.trace_path.clone())?;
    let _state_file = write_state(mount_point, server.addr(), options)?;
    mount_client(mount_point, server.addr())?;

    tracing::info!(
        mount = %mount_point.display(),
        addr = %server.addr(),
        "NFS loopback mount established"
    );

    match wait_for_mount_exit(mount_point, signal_rx)? {
        MountExit::Unmounted => {
            tracing::info!("NFS mount exited");
        },
        MountExit::Interrupted => {
            tracing::info!("NFS mount interrupted and unmounted");
        },
    }

    drop(server);
    Ok(())
}

pub fn unmount(mount_point: &Path) -> Result<(), NfsFrontendError> {
    UnmountCommand::for_platform(mount_point).run()
}

pub fn read_mount_states(state_dir: &Path) -> Result<Vec<NfsMountState>, NfsFrontendError> {
    let entries = match std::fs::read_dir(state_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
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

    let mut states = Vec::new();
    for path in paths {
        match read_mount_state_file(&path) {
            Ok(Some(state)) => states.push(state),
            Ok(None) => {},
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %error,
                    "failed to read NFS mount state file"
                );
            },
        }
    }
    Ok(states)
}

fn read_mount_state_file(path: &Path) -> Result<Option<NfsMountState>, NfsFrontendError> {
    let file = std::fs::File::open(path)?;
    let state = serde_json::from_reader::<_, NfsMountState>(file)
        .map_err(|error| NfsFrontendError::State(error.to_string()))?;
    if state.version == NfsMountState::VERSION {
        Ok(Some(state))
    } else {
        Ok(None)
    }
}

fn mount_client(mount_point: &Path, addr: SocketAddr) -> Result<(), NfsFrontendError> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        MountCommand::for_platform(mount_point, addr).run()
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (mount_point, addr);
        Err(NfsFrontendError::Mount(
            "automatic NFSv4 mount is not implemented on this platform".to_string(),
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UnmountCommand {
    program: &'static str,
    args: Vec<OsString>,
    failure_context: &'static str,
}

impl UnmountCommand {
    #[cfg(target_os = "macos")]
    fn for_platform(mount_point: &Path) -> Self {
        Self::macos(mount_point)
    }

    #[cfg(target_os = "linux")]
    fn for_platform(mount_point: &Path) -> Self {
        Self::linux(mount_point)
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    fn for_platform(mount_point: &Path) -> Self {
        let _ = mount_point;
        Self {
            program: "false",
            args: Vec::new(),
            failure_context: "automatic NFS unmount is not implemented on this platform",
        }
    }

    #[cfg(target_os = "macos")]
    fn macos(mount_point: &Path) -> Self {
        Self {
            program: "diskutil",
            args: vec![
                OsString::from("unmount"),
                mount_point.as_os_str().to_owned(),
            ],
            failure_context: "diskutil unmount",
        }
    }

    #[cfg(target_os = "linux")]
    fn linux(mount_point: &Path) -> Self {
        Self {
            program: "umount",
            args: vec![mount_point.as_os_str().to_owned()],
            failure_context: "umount",
        }
    }

    fn run(&self) -> Result<(), NfsFrontendError> {
        let status = Command::new(self.program)
            .args(&self.args)
            .status()
            .map_err(|error| NfsFrontendError::Unmount(error.to_string()))?;
        if status.success() {
            Ok(())
        } else {
            Err(NfsFrontendError::Unmount(format!(
                "{} exited with {status}",
                self.failure_context
            )))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MountCommand {
    program: &'static str,
    args: Vec<OsString>,
    failure_context: &'static str,
}

impl MountCommand {
    #[cfg(target_os = "macos")]
    fn for_platform(mount_point: &Path, addr: SocketAddr) -> Self {
        Self::macos(mount_point, addr.port())
    }

    #[cfg(target_os = "linux")]
    fn for_platform(mount_point: &Path, addr: SocketAddr) -> Self {
        Self::linux(mount_point, addr.port())
    }

    #[cfg(any(target_os = "macos", test))]
    fn macos(mount_point: &Path, port: u16) -> Self {
        Self {
            program: "sudo",
            args: vec![
                OsString::from("-n"),
                OsString::from("mount_nfs"),
                OsString::from("-o"),
                OsString::from(MountOptions::macos(port).render()),
                OsString::from(export_source()),
                mount_point.as_os_str().to_owned(),
            ],
            failure_context: "mount_nfs via `sudo -n` (a password prompt needs `sudo -v` first; \
                              an `Invalid argument` above means an unsupported mount option)",
        }
    }

    #[cfg(any(target_os = "linux", test))]
    fn linux(mount_point: &Path, port: u16) -> Self {
        Self {
            program: "mount",
            args: vec![
                OsString::from("-t"),
                OsString::from("nfs4"),
                OsString::from("-o"),
                OsString::from(MountOptions::linux(port).render()),
                OsString::from(export_source()),
                mount_point.as_os_str().to_owned(),
            ],
            failure_context: "mount",
        }
    }

    fn run(&self) -> Result<(), NfsFrontendError> {
        let status = Command::new(self.program)
            .args(&self.args)
            .status()
            .map_err(|error| NfsFrontendError::Mount(error.to_string()))?;

        if status.success() {
            Ok(())
        } else {
            Err(NfsFrontendError::Mount(format!(
                "{} exited with {status}",
                self.failure_context
            )))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MountOption {
    value: String,
    rationale: &'static str,
}

impl MountOption {
    fn new(value: impl Into<String>, rationale: &'static str) -> Self {
        Self {
            value: value.into(),
            rationale,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MountOptions {
    options: Vec<MountOption>,
}

impl MountOptions {
    #[cfg(any(target_os = "macos", test))]
    fn macos(port: u16) -> Self {
        Self {
            options: vec![
                MountOption::new("vers=4", "use the NFSv4 protocol subset implemented here"),
                MountOption::new("tcp", "match the loopback TCP listener"),
                MountOption::new(
                    format!("port={port}"),
                    "connect to the ephemeral loopback server port",
                ),
                MountOption::new("sec=sys", "use local AUTH_SYS credentials only"),
                MountOption::new("ro", "preserve omnifs' read-only provider contract"),
                MountOption::new(
                    "intr",
                    "allow interrupted client operations during teardown",
                ),
                MountOption::new("nocallback", "disable delegations and callback traffic"),
                MountOption::new("noac", "avoid kernel attribute caching while attrs mature"),
                MountOption::new(
                    "nonegnamecache",
                    "avoid stale negative lookup caching for provider-backed paths",
                ),
                MountOption::new(
                    "retrycnt=0",
                    "fail the mount promptly when loopback setup is wrong",
                ),
                MountOption::new("timeo=5", "bound client wait time for a local server"),
                MountOption::new("retrans=1", "avoid long retry tails on local failures"),
            ],
        }
    }

    #[cfg(any(target_os = "linux", test))]
    fn linux(port: u16) -> Self {
        Self {
            options: vec![
                MountOption::new(
                    "vers=4.0",
                    "use the NFSv4.0 protocol subset implemented here",
                ),
                MountOption::new("proto=tcp", "match the loopback TCP listener"),
                MountOption::new(
                    format!("port={port}"),
                    "connect to the ephemeral loopback server port",
                ),
                MountOption::new("ro", "preserve omnifs' read-only provider contract"),
                MountOption::new(
                    "soft",
                    "avoid indefinite hangs against the local test server",
                ),
                MountOption::new("timeo=5", "bound client wait time for a local server"),
                MountOption::new("retrans=1", "avoid long retry tails on local failures"),
                MountOption::new(
                    "lookupcache=none",
                    "force lookups through the NFS frontend while invalidation matures",
                ),
                MountOption::new(
                    "actimeo=0",
                    "disable Linux attribute-cache retention during bring-up",
                ),
            ],
        }
    }

    fn render(self) -> String {
        self.options
            .into_iter()
            .map(|option| {
                assert!(
                    !option.rationale.is_empty(),
                    "mount options must document their correctness or performance rationale"
                );
                option.value
            })
            .collect::<Vec<_>>()
            .join(",")
    }
}

fn export_source() -> String {
    format!("127.0.0.1:/{}", crate::protocol::consts::NFS_EXPORT_NAME)
}

#[cfg(target_os = "macos")]
fn mount_table_entries() -> std::io::Result<Vec<MountTableEntry>> {
    let output = Command::new("mount").output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "mount exited with {}",
            output.status
        )));
    }
    Ok(parse_macos_mounts(&String::from_utf8_lossy(&output.stdout)))
}

#[cfg(target_os = "linux")]
fn mount_table_entries() -> std::io::Result<Vec<MountTableEntry>> {
    std::fs::read_to_string("/proc/mounts").map(|mounts| parse_proc_mounts(&mounts))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn mount_table_entries() -> std::io::Result<Vec<MountTableEntry>> {
    Ok(Vec::new())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MountTableEntry {
    mount_point: PathBuf,
}

#[cfg(any(target_os = "linux", test))]
fn parse_proc_mounts(contents: &str) -> Vec<MountTableEntry> {
    contents
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let _source = fields.next()?;
            let mount_point = fields.next()?;
            Some(MountTableEntry {
                mount_point: PathBuf::from(decode_proc_mount_field(mount_point)),
            })
        })
        .collect()
}

#[cfg(any(target_os = "linux", test))]
fn decode_proc_mount_field(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = String::with_capacity(field.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 3 < bytes.len()
            && bytes[i + 1].is_ascii_digit()
            && bytes[i + 2].is_ascii_digit()
            && bytes[i + 3].is_ascii_digit()
        {
            let octal = &field[i + 1..i + 4];
            if let Ok(value) = u8::from_str_radix(octal, 8) {
                out.push(char::from(value));
                i += 4;
                continue;
            }
        }

        out.push(char::from(bytes[i]));
        i += 1;
    }
    out
}

#[cfg(any(target_os = "macos", test))]
fn parse_macos_mounts(contents: &str) -> Vec<MountTableEntry> {
    contents
        .lines()
        .filter_map(|line| {
            let (mount, _options) = line.rsplit_once(" (")?;
            let (_source, mount_point) = mount.rsplit_once(" on ")?;
            Some(MountTableEntry {
                mount_point: PathBuf::from(mount_point),
            })
        })
        .collect()
}

/// Whether an NFS mount is currently active at `mount_point`, read from the
/// live OS mount table (`/proc/mounts` on Linux, `mount` on macOS). The daemon
/// uses this for readiness on hosts without `/proc`.
pub fn mount_is_active(mount_point: &Path) -> bool {
    match mount_table_entries() {
        Ok(entries) => mount_table_contains(&entries, mount_point),
        Err(error) => {
            tracing::warn!(
                mount = %mount_point.display(),
                error = %error,
                "failed to inspect mount table"
            );
            false
        },
    }
}

fn mount_table_contains(entries: &[MountTableEntry], mount_point: &Path) -> bool {
    let wanted = normalize_mount_path(mount_point);
    let canonical = normalize_mount_path(&canonical_mount_path(mount_point));
    entries.iter().any(|entry| {
        let entry_path = normalize_mount_path(&entry.mount_point);
        entry_path == wanted || entry_path == canonical
    })
}

fn canonical_mount_path(path: &Path) -> PathBuf {
    // Resolve symlinks (e.g. /var -> /private/var) via the PARENT and rejoin the
    // leaf, never stat-ing the path itself: a stat on a dead-server NFS mount
    // hangs uninterruptibly, which would wedge every `mount_is_active` check
    // during teardown of a crashed daemon.
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(leaf)) => std::fs::canonicalize(parent)
            .map_or_else(|_| path.to_path_buf(), |parent| parent.join(leaf)),
        _ => path.to_path_buf(),
    }
}

fn normalize_mount_path(path: &Path) -> PathBuf {
    path.components().collect()
}

enum MountExit {
    Unmounted,
    Interrupted,
}

fn ctrl_c_receiver(rt: &Handle) -> mpsc::Receiver<()> {
    let (tx, rx) = mpsc::channel();
    std::mem::drop(rt.spawn(async move {
        match tokio::signal::ctrl_c().await {
            Ok(()) => {
                let _ = tx.send(());
            },
            Err(error) => {
                tracing::warn!(error = %error, "failed to register Ctrl-C handler");
            },
        }
    }));
    rx
}

fn wait_for_mount_exit(
    mount_point: &Path,
    signal_rx: mpsc::Receiver<()>,
) -> Result<MountExit, NfsFrontendError> {
    let mut signal_rx = Some(signal_rx);

    loop {
        if !mount_is_active(mount_point) {
            return Ok(MountExit::Unmounted);
        }

        if wait_interval_or_signal(&mut signal_rx) {
            tracing::info!(
                mount = %mount_point.display(),
                "Ctrl-C received, unmounting NFS loopback mount"
            );
            unmount(mount_point)?;
            wait_until_inactive(mount_point)?;
            return Ok(MountExit::Interrupted);
        }
    }
}

fn wait_interval_or_signal(signal_rx: &mut Option<mpsc::Receiver<()>>) -> bool {
    let Some(rx) = signal_rx else {
        thread::sleep(MOUNT_WAIT_INTERVAL);
        return false;
    };

    match rx.recv_timeout(MOUNT_WAIT_INTERVAL) {
        Ok(()) => true,
        Err(mpsc::RecvTimeoutError::Timeout) => false,
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            *signal_rx = None;
            false
        },
    }
}

fn wait_until_inactive(mount_point: &Path) -> Result<(), NfsFrontendError> {
    let deadline = Instant::now() + UNMOUNT_SETTLE_TIMEOUT;
    while mount_is_active(mount_point) {
        if Instant::now() >= deadline {
            return Err(NfsFrontendError::Unmount(format!(
                "{} remained mounted after unmount",
                mount_point.display()
            )));
        }
        thread::sleep(UNMOUNT_SETTLE_INTERVAL);
    }
    Ok(())
}

fn ensure_private_state_dir(state_dir: &Path) -> Result<(), NfsFrontendError> {
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

#[derive(Debug)]
struct StateFile {
    path: PathBuf,
}

impl Drop for StateFile {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                path = %self.path.display(),
                error = %error,
                "failed to remove NFS mount state file"
            );
        }
    }
}

fn write_state(
    mount_point: &Path,
    addr: SocketAddr,
    mount_options: &NfsMountOptions,
) -> Result<StateFile, NfsFrontendError> {
    let state_dir = &mount_options.state_dir;
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
    serde_json::to_writer_pretty(&mut file, &state)
        .map_err(|error| NfsFrontendError::State(error.to_string()))?;
    writeln!(file)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(STATE_FILE_MODE))?;
    }
    Ok(StateFile { path })
}

fn sweep_stale_states(state_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(state_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Ok(Some(state)) = read_mount_state_file(&path) else {
            continue;
        };
        if !pid_alive(state.pid) {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn pid_alive(pid: u32) -> bool {
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    use super::{
        MountCommand, MountOptions, MountTableEntry, NfsMountOptions, ensure_private_state_dir,
        mount_table_contains, parse_macos_mounts, parse_proc_mounts, read_mount_states,
        write_state,
    };
    use serde_json::Value;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::path::{Path, PathBuf};

    fn args_as_strings(command: &MountCommand) -> Vec<String> {
        command
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn linux_mount_command_uses_documented_loopback_options() {
        let command = MountCommand::linux(Path::new("/mnt/omnifs"), 2049);
        assert_eq!(command.program, "mount");
        assert_eq!(
            args_as_strings(&command),
            vec![
                "-t",
                "nfs4",
                "-o",
                "vers=4.0,proto=tcp,port=2049,ro,soft,timeo=5,retrans=1,lookupcache=none,actimeo=0",
                "127.0.0.1:/omnifs",
                "/mnt/omnifs",
            ]
        );
    }

    #[test]
    fn macos_mount_command_uses_documented_loopback_options() {
        let command = MountCommand::macos(Path::new("/Volumes/omnifs"), 2050);
        assert_eq!(command.program, "sudo");
        assert_eq!(
            args_as_strings(&command),
            vec![
                "-n",
                "mount_nfs",
                "-o",
                "vers=4,tcp,port=2050,sec=sys,ro,intr,nocallback,noac,nonegnamecache,retrycnt=0,timeo=5,retrans=1",
                "127.0.0.1:/omnifs",
                "/Volumes/omnifs",
            ]
        );
    }

    #[test]
    fn mount_options_do_not_carry_secret_material() {
        for option in MountOptions::linux(2049)
            .render()
            .split(',')
            .chain(MountOptions::macos(2049).render().split(','))
        {
            let option = option.to_ascii_lowercase();
            assert!(!option.contains("token"));
            assert!(!option.contains("secret"));
            assert!(!option.contains("password"));
            assert!(!option.contains("passwd"));
            assert!(!option.contains("key"));
        }
    }

    #[test]
    fn proc_mounts_decode_exact_mount_points() {
        let mounts = parse_proc_mounts("127.0.0.1:/omnifs /tmp/omnifs\\040mount nfs4 rw 0 0\n");
        assert_eq!(
            mounts,
            vec![MountTableEntry {
                mount_point: PathBuf::from("/tmp/omnifs mount"),
            }]
        );
        assert!(mount_table_contains(
            &mounts,
            Path::new("/tmp/omnifs mount")
        ));
        assert!(!mount_table_contains(&mounts, Path::new("/tmp/omnifs")));
    }

    #[test]
    fn macos_mount_parser_extracts_exact_mount_points() {
        let mounts =
            parse_macos_mounts("127.0.0.1:/omnifs on /Volumes/omnifs mount (nfs, nodev, noexec)\n");
        assert_eq!(
            mounts,
            vec![MountTableEntry {
                mount_point: PathBuf::from("/Volumes/omnifs mount"),
            }]
        );
        assert!(mount_table_contains(
            &mounts,
            Path::new("/Volumes/omnifs mount")
        ));
        assert!(!mount_table_contains(&mounts, Path::new("/Volumes/omnifs")));
    }

    #[test]
    fn state_file_is_json_and_removed_on_drop() {
        let temp = tempfile::tempdir().expect("tempdir");
        ensure_private_state_dir(temp.path()).expect("state dir");
        let options = NfsMountOptions::loopback(temp.path().to_path_buf());
        let guard = write_state(
            Path::new("/mnt/omnifs"),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 2049),
            &options,
        )
        .expect("state file");
        let path = guard.path.clone();
        let state: Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read state")).expect("json");

        assert_eq!(state["version"], 1);
        assert_eq!(state["mount_point"], "/mnt/omnifs");
        assert_eq!(state["addr"], "127.0.0.1:2049");
        assert!(state["pid"].as_u64().is_some());
        let states = read_mount_states(temp.path()).expect("mount states");
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].mount_point, PathBuf::from("/mnt/omnifs"));

        drop(guard);
        assert!(!path.exists());
    }
}
