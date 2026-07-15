use crate::adapter::Export;
use crate::error::NfsFrontendError;
use crate::persist::{FH_STATE_FILE, FhState, PersistInit};
use crate::protocol::consts::EXPORT_ROOT_ID;
use crate::server::start_server;
use omnifs_engine::namespace::Namespace;
#[cfg(target_os = "linux")]
use omnifs_mtab::proc_mounts;
use omnifs_mtab::{MountState, Platform, StateError, StateFile, UnmountCommand};
use std::ffi::OsString;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tokio::runtime::Handle;

const MOUNT_WAIT_INTERVAL: Duration = Duration::from_millis(500);

#[cfg(unix)]
const STATE_DIR_MODE: u32 = 0o700;
#[derive(Debug, Clone)]
pub struct NfsMountOptions {
    pub bind: SocketAddr,
    pub trace_path: Option<PathBuf>,
    pub state_dir: PathBuf,
    /// Persist the filehandle-identity table so a restart of this process decodes
    /// the filehandles a kernel client still holds. Set by the restartable
    /// out-of-process runner; left `false` for callers that do not need restart
    /// persistence. When `true` the mount step is also skipped if the mount point
    /// already carries an active NFS mount (the restart case), so the restarted
    /// server serves the export the kernel client is still connected to instead
    /// of remounting.
    pub persist_filehandles: bool,
}

impl NfsMountOptions {
    pub fn loopback(state_dir: PathBuf) -> Self {
        Self {
            bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            trace_path: None,
            state_dir,
            persist_filehandles: false,
        }
    }

    /// Resolve the optional persisted filehandle table seed.
    fn persist_init(&self) -> Result<Option<PersistInit>, NfsFrontendError> {
        if !self.persist_filehandles {
            return Ok(None);
        }
        let state_path = self.state_dir.join(FH_STATE_FILE);
        let (generation, next_ino, entries) = match FhState::load(&state_path)
            .map_err(|error| NfsFrontendError::State(error.to_string()))?
        {
            Some(state) => (state.generation, state.next_ino, state.entries),
            None => (
                crate::protocol::filehandle::generation(),
                EXPORT_ROOT_ID + 1,
                Vec::new(),
            ),
        };
        Ok(Some(PersistInit {
            generation,
            next_ino,
            entries,
            state_path,
        }))
    }

    fn bind_for_mount(&self, mount_point: &Path) -> Result<SocketAddr, NfsFrontendError> {
        if !self.persist_filehandles || !mount_is_active_checked(mount_point)? {
            return Ok(self.bind);
        }
        self.bind_for_active_mount(mount_point)
    }

    fn bind_for_active_mount(&self, mount_point: &Path) -> Result<SocketAddr, NfsFrontendError> {
        let persisted = MountState::read_unique(&self.state_dir)
            .and_then(|state| state.nfs_addr_for(mount_point))
            .map_err(|error| NfsFrontendError::State(error.to_string()))?;
        if self.bind.port() != 0 && self.bind != persisted {
            return Err(NfsFrontendError::State(format!(
                "active NFS mount {} is connected to {persisted}, not requested {}",
                mount_point.display(),
                self.bind
            )));
        }
        Ok(persisted)
    }
}

pub fn mount_blocking(
    mount_point: &Path,
    namespace: Arc<dyn Namespace>,
    rt: Handle,
    options: &NfsMountOptions,
) -> Result<(), NfsFrontendError> {
    std::fs::create_dir_all(mount_point)?;
    ensure_private_state_dir(&options.state_dir)?;
    let bind = options.bind_for_mount(mount_point)?;

    // A pinned filehandle generation persists across a restart so a kernel client
    // never sees `NFS4ERR_FHEXPIRED` for a handle it still holds. Off the runner
    // path, keep the fresh-per-process random generation.
    let persist_init = options.persist_init()?;

    let export = Arc::new(match persist_init {
        Some(init) => Export::with_persistence(rt, namespace, init),
        None => Export::new(rt, namespace),
    });

    let server = start_server(
        Arc::clone(&export) as Arc<dyn crate::export::ReadOnlyExport>,
        bind,
        options.trace_path.clone(),
    )?;
    let state_file =
        StateFile::write_nfs(mount_point, server.addr(), &options.state_dir).map_err(|error| {
            match error {
                StateError::Io(error) => error.into(),
                error => NfsFrontendError::State(error.to_string()),
            }
        })?;
    MountState::remove_other_files(&options.state_dir, state_file.path())
        .map_err(|error| NfsFrontendError::State(error.to_string()))?;

    // Restart case: the kernel client still holds the mount, so serve the export
    // over the same port without remounting. A first start (or a stale, dead
    // mount) mounts as usual.
    if options.persist_filehandles && mount_is_active(mount_point) {
        tracing::info!(
            mount = %mount_point.display(),
            addr = %server.addr(),
            "NFS mount already active; serving the export without remounting (restart path)"
        );
    } else {
        mount_client(mount_point, server.addr())?;
    }

    disable_spotlight(mount_point);

    tracing::info!(
        mount = %mount_point.display(),
        addr = %server.addr(),
        "NFS loopback mount established"
    );

    wait_for_mount_exit(mount_point);
    drop(server);
    Ok(())
}

pub fn unmount(mount_point: &Path) -> Result<(), NfsFrontendError> {
    UnmountCommand::nfs_graceful(Platform::current(), mount_point)
        .run()
        .map_err(|error| NfsFrontendError::Unmount(error.to_string()))
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
                "{} exited with {}",
                self.failure_context, status
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
                    "nobrowse",
                    "keep Finder from presenting the projected mount as a browsable volume",
                ),
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

#[cfg(any(target_os = "macos", test))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct SpotlightCommand {
    program: &'static str,
    args: Vec<OsString>,
    failure_context: &'static str,
}

#[cfg(any(target_os = "macos", test))]
impl SpotlightCommand {
    fn macos(mount_point: &Path) -> Self {
        Self {
            program: "sudo",
            args: vec![
                OsString::from("-n"),
                OsString::from("mdutil"),
                OsString::from("-d"),
                OsString::from("-i"),
                OsString::from("off"),
                mount_point.as_os_str().to_owned(),
            ],
            failure_context: "mdutil Spotlight exclusion via `sudo -n`",
        }
    }

    fn run(&self) -> Result<(), NfsFrontendError> {
        let output = Command::new(self.program)
            .args(&self.args)
            .output()
            .map_err(|error| NfsFrontendError::Mount(error.to_string()))?;
        let details = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if output.status.success() || details.contains("Indexing and searching disabled.") {
            Ok(())
        } else {
            Err(NfsFrontendError::Mount(format!(
                "{} exited with {}",
                self.failure_context, output.status
            )))
        }
    }
}

fn disable_spotlight(mount_point: &Path) {
    #[cfg(target_os = "macos")]
    match SpotlightCommand::macos(mount_point).run() {
        Ok(()) => tracing::info!(
            mount = %mount_point.display(),
            "disabled Spotlight activity and indexing"
        ),
        Err(error) => tracing::warn!(
            mount = %mount_point.display(),
            %error,
            "could not disable Spotlight activity and indexing"
        ),
    }

    #[cfg(not(target_os = "macos"))]
    let _ = mount_point;
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
    std::fs::read_to_string("/proc/mounts").map(|mounts| {
        proc_mounts::parse(&mounts)
            .into_iter()
            .map(|entry| MountTableEntry {
                source: entry.device,
                mount_point: PathBuf::from(entry.mount_point),
                fs_type: entry.fs_type,
            })
            .collect()
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn mount_table_entries() -> std::io::Result<Vec<MountTableEntry>> {
    Ok(Vec::new())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MountTableEntry {
    source: String,
    mount_point: PathBuf,
    fs_type: String,
}

#[cfg(any(target_os = "macos", test))]
fn parse_macos_mounts(contents: &str) -> Vec<MountTableEntry> {
    contents
        .lines()
        .filter_map(|line| {
            let (mount, options) = line.rsplit_once(" (")?;
            let (source, mount_point) = mount.rsplit_once(" on ")?;
            let fs_type = options
                .trim_start_matches(" (")
                .split(',')
                .next()?
                .to_string();
            Some(MountTableEntry {
                source: source.to_string(),
                mount_point: PathBuf::from(mount_point),
                fs_type,
            })
        })
        .collect()
}

/// Whether an NFS mount is currently active at `mount_point`, read from the
/// live OS mount table (`/proc/mounts` on Linux, `mount` on macOS). The daemon
/// uses this for readiness on hosts without `/proc`.
pub fn mount_is_active(mount_point: &Path) -> bool {
    match mount_is_active_checked(mount_point) {
        Ok(active) => active,
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

/// Whether `mount_point` is the loopback NFS export served by an Omnifs NFS
/// runner. Teardown uses this before acting on a stale state file so an old or
/// corrupted record cannot unmount an unrelated filesystem.
pub fn mount_is_omnifs(mount_point: &Path) -> bool {
    mount_table_entries().is_ok_and(|entries| {
        entries.iter().any(|entry| {
            mount_entry_matches(entry, mount_point)
                && entry.source == export_source()
                && entry.fs_type.starts_with("nfs")
        })
    })
}

/// Fallible mount-table probe for lifecycle decisions that must fail closed.
pub fn mount_is_active_checked(mount_point: &Path) -> Result<bool, NfsFrontendError> {
    mount_table_entries()
        .map(|entries| mount_table_contains(&entries, mount_point))
        .map_err(Into::into)
}

fn mount_table_contains(entries: &[MountTableEntry], mount_point: &Path) -> bool {
    entries
        .iter()
        .any(|entry| mount_entry_matches(entry, mount_point))
}

fn mount_entry_matches(entry: &MountTableEntry, mount_point: &Path) -> bool {
    let wanted = normalize_mount_path(mount_point);
    let canonical = normalize_mount_path(&canonical_mount_path(mount_point));
    let entry_path = normalize_mount_path(&entry.mount_point);
    entry_path == wanted || entry_path == canonical
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

fn wait_for_mount_exit(mount_point: &Path) {
    loop {
        match mount_is_active_checked(mount_point) {
            Ok(false) => {
                tracing::info!("NFS mount exited");
                return;
            },
            Ok(true) => {},
            Err(error) => {
                tracing::warn!(
                    mount = %mount_point.display(),
                    %error,
                    "failed to inspect mount table; keeping NFS frontend alive"
                );
            },
        }
        thread::sleep(MOUNT_WAIT_INTERVAL);
    }
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

#[cfg(test)]
mod tests {
    use super::{
        MountCommand, MountOptions, MountTableEntry, NfsMountOptions, SpotlightCommand,
        mount_is_active_checked, mount_table_contains, parse_macos_mounts,
    };
    use omnifs_mtab::StateFile;
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
                "vers=4,tcp,port=2050,sec=sys,ro,nobrowse,intr,nocallback,noac,nonegnamecache,retrycnt=0,timeo=5,retrans=1",
                "127.0.0.1:/omnifs",
                "/Volumes/omnifs",
            ]
        );
    }

    #[test]
    fn macos_spotlight_command_disables_search_and_indexing() {
        let command = SpotlightCommand::macos(Path::new("/Volumes/omnifs"));
        assert_eq!(command.program, "sudo");
        assert_eq!(
            command
                .args
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            vec!["-n", "mdutil", "-d", "-i", "off", "/Volumes/omnifs",]
        );
    }

    #[test]
    fn active_mount_reuses_persisted_server_address() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mount = Path::new("/mnt/omnifs");
        let persisted = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 2049);
        let _state = StateFile::write_nfs(mount, persisted, dir.path()).expect("state");

        let mut options = NfsMountOptions::loopback(dir.path().to_path_buf());
        options.persist_filehandles = true;
        assert_eq!(options.bind_for_active_mount(mount).unwrap(), persisted);

        options.bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 2050);
        assert!(options.bind_for_active_mount(mount).is_err());
    }

    #[test]
    fn active_mount_rejects_ambiguous_server_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mount = Path::new("/mnt/omnifs");
        let _first = StateFile::write_nfs(
            mount,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 2049),
            dir.path(),
        )
        .expect("first state");
        let _second = StateFile::write_nfs(
            mount,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 2050),
            dir.path(),
        )
        .expect("second state");

        let mut options = NfsMountOptions::loopback(dir.path().to_path_buf());
        options.persist_filehandles = true;
        assert!(options.bind_for_active_mount(mount).is_err());
    }

    #[test]
    fn active_mount_rejects_fuse_or_other_mount_state() {
        let fuse_dir = tempfile::tempdir().expect("tempdir");
        let _fuse = StateFile::write_fuse(Path::new("/mnt/omnifs"), fuse_dir.path()).unwrap();
        let mut options = NfsMountOptions::loopback(fuse_dir.path().to_path_buf());
        options.persist_filehandles = true;
        assert!(
            options
                .bind_for_active_mount(Path::new("/mnt/omnifs"))
                .is_err()
        );

        let other_dir = tempfile::tempdir().expect("tempdir");
        let _other = StateFile::write_nfs(
            Path::new("/mnt/other"),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 2049),
            other_dir.path(),
        )
        .unwrap();
        let mut options = NfsMountOptions::loopback(other_dir.path().to_path_buf());
        options.persist_filehandles = true;
        assert!(
            options
                .bind_for_active_mount(Path::new("/mnt/omnifs"))
                .is_err()
        );
    }

    #[test]
    fn checked_mount_probe_reports_absent_mount() {
        assert!(!mount_is_active_checked(Path::new("/definitely/not/an/omnifs/mount")).unwrap());
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
    fn macos_mount_parser_extracts_exact_mount_points() {
        let mounts =
            parse_macos_mounts("127.0.0.1:/omnifs on /Volumes/omnifs mount (nfs, nodev, noexec)\n");
        assert_eq!(
            mounts,
            vec![MountTableEntry {
                source: "127.0.0.1:/omnifs".to_string(),
                mount_point: PathBuf::from("/Volumes/omnifs mount"),
                fs_type: "nfs".to_string(),
            }]
        );
        assert!(mount_table_contains(
            &mounts,
            Path::new("/Volumes/omnifs mount")
        ));
        assert!(!mount_table_contains(&mounts, Path::new("/Volumes/omnifs")));
    }

    #[test]
    fn reload_keeps_generation_and_resumes_cursor() {
        use crate::export::NodeKind;
        use crate::persist::{FH_STATE_FILE, FhEntry, FhState};

        let dir = tempfile::tempdir().expect("state dir");
        super::ensure_private_state_dir(dir.path()).expect("state dir perms");
        let persisted = FhState {
            version: FhState::VERSION,
            generation: 0xABCD_1234,
            next_ino: 512,
            entries: vec![FhEntry {
                id: 100,
                scope: 1,
                parent: 1,
                name: "test".to_string(),
                kind: NodeKind::Directory,
                path: omnifs_core::path::Path::parse("/test").unwrap(),
            }],
        };
        std::fs::write(
            dir.path().join(FH_STATE_FILE),
            serde_json::to_vec(&persisted).expect("encode"),
        )
        .expect("write filehandle state");

        let mut options = super::NfsMountOptions::loopback(dir.path().to_path_buf());
        options.persist_filehandles = true;
        let init = options
            .persist_init()
            .expect("resolve generation")
            .expect("persist init on the runner path");
        assert_eq!(
            init.generation, 0xABCD_1234,
            "a reload must keep the generation"
        );
        assert_eq!(init.next_ino, 512, "a reload resumes the allocation cursor");
        assert_eq!(init.entries.len(), 1);
    }

    #[test]
    fn no_persist_has_no_persisted_seed() {
        let dir = tempfile::tempdir().expect("state dir");
        let options = super::NfsMountOptions::loopback(dir.path().to_path_buf());
        assert!(
            options
                .persist_init()
                .expect("resolve persisted seed")
                .is_none(),
            "the daemon path never persists filehandles"
        );
    }
}
