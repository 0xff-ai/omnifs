use std::fs::{File, OpenOptions};
use std::io::{BufRead as _, Read as _, Write as _};
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::api::{Api, Disk, Feature, LibraryApi, PortDirection, ShutdownFd};
use crate::{CONTROL_MAX_LINE_BYTES, Config, Error, HelperRecord};

pub fn run(config: &Config) -> Result<(), Error> {
    if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        return Err(Error::UnsupportedPlatform);
    }
    config.validate_runtime_inputs()?;
    let diagnostic = config.open_diagnostic_log()?;
    let api = LibraryApi::load(&config.library)?;
    run_with(&api, config, &diagnostic)
}

fn run_with<A: Api>(api: &A, config: &Config, diagnostic: &File) -> Result<(), Error> {
    api.init_log(diagnostic.as_raw_fd())?;
    let bridge = AttachBridge::bind(&config.attach_bridge_socket, &config.attach_socket)?;
    let mut context = Context::configure(api, config)?;
    let control = Control::bind(
        &config.control_socket,
        &config.filesystem,
        &config.spec,
        &config.instance_id,
    )?;
    let shutdown_fd = context.shutdown_fd()?;
    let files = PublishedPid::publish(
        &config.pid_file,
        &config.filesystem,
        &config.spec,
        &config.instance_id,
    )?;
    let stop = Arc::new(AtomicBool::new(false));
    let control_thread = control.spawn(shutdown_fd, Arc::clone(&stop));
    let bridge_thread = bridge.spawn(shutdown_fd, Arc::clone(&stop));

    let result = context.start();
    bridge_thread.stop();
    let _ = control_thread.join();
    bridge_thread.join();
    drop(files);
    result
}

struct Context<'a, A: Api> {
    api: &'a A,
    id: u32,
    active: bool,
}

impl<'a, A: Api> Context<'a, A> {
    fn configure(api: &'a A, config: &Config) -> Result<Self, Error> {
        for feature in [Feature::Block, Feature::Efi] {
            if !api.has_feature(feature)? {
                return Err(Error::Config(format!(
                    "packaged libkrun lacks required {} support",
                    feature.name()
                )));
            }
        }
        if api.has_feature(Feature::Gpu)? {
            return Err(Error::Config(
                "packaged libkrun unexpectedly includes GPU support".to_owned(),
            ));
        }

        let context = Self {
            api,
            id: api.create_context()?,
            active: true,
        };
        context.api.set_firmware(context.id, &config.firmware)?;
        context
            .api
            .set_vm_config(context.id, config.vcpus, config.memory_mib)?;
        context
            .api
            .add_disk(context.id, Disk::Root, &config.root_disk)?;
        context
            .api
            .add_disk(context.id, Disk::Seed, &config.seed_disk)?;
        context.api.disable_implicit_vsock(context.id)?;
        context.api.add_vsock(context.id)?;
        context.api.add_vsock_port(
            context.id,
            config.attach_port,
            &config.attach_bridge_socket,
            PortDirection::GuestConnects,
        )?;
        context.api.add_vsock_port(
            context.id,
            config.readiness_port,
            &config.readiness_socket,
            PortDirection::GuestConnects,
        )?;
        context.api.add_vsock_port(
            context.id,
            config.ssh_port,
            &config.ssh_socket,
            PortDirection::HostConnects,
        )?;
        context
            .api
            .set_console_output(context.id, &config.serial_log)?;
        Ok(context)
    }

    fn shutdown_fd(&self) -> Result<ShutdownFd, Error> {
        self.api.shutdown_fd(self.id)
    }

    fn start(&mut self) -> Result<(), Error> {
        self.active = false;
        let value = self.api.start_enter(self.id);
        Err(if value < 0 {
            Error::Call {
                function: "krun_start_enter",
                code: value,
            }
        } else {
            Error::UnexpectedReturn {
                function: "krun_start_enter",
                value,
            }
        })
    }
}

impl<A: Api> Drop for Context<'_, A> {
    fn drop(&mut self) {
        if self.active {
            self.api.free_context(self.id);
        }
    }
}

/// Keeps libkrun connected to a helper-owned socket whose peer can be closed
/// when the daemon replaces its own attach listener. libkrun then resets the
/// guest stream, and the wire client opens a fresh connection through this
/// same bridge to the restored daemon socket.
struct AttachBridge {
    path: PathBuf,
    target: PathBuf,
    listener: UnixListener,
}

struct AttachBridgeThread {
    join: thread::JoinHandle<()>,
    active: Arc<Mutex<Option<ActiveRelay>>>,
    stop: Arc<AtomicBool>,
}

struct ActiveRelay {
    guest: UnixStream,
    daemon: UnixStream,
}

impl AttachBridgeThread {
    fn stop(&self) {
        use std::net::Shutdown;

        self.stop.store(true, Ordering::Release);
        let active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(active) = active.as_ref() {
            let _ = active.guest.shutdown(Shutdown::Both);
            let _ = active.daemon.shutdown(Shutdown::Both);
        }
    }

    fn join(self) {
        let _ = self.join.join();
    }
}

impl AttachBridge {
    fn bind(path: &Path, target: &Path) -> Result<Self, Error> {
        let listener = UnixListener::bind(path)
            .map_err(|error| Error::io("bind attach bridge socket", path, error))?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| Error::io("restrict attach bridge socket", path, error))?;
        listener
            .set_nonblocking(true)
            .map_err(|error| Error::io("set attach bridge socket nonblocking", path, error))?;
        Ok(Self {
            path: path.to_path_buf(),
            target: target.to_path_buf(),
            listener,
        })
    }

    fn spawn(self, shutdown_fd: ShutdownFd, stop: Arc<AtomicBool>) -> AttachBridgeThread {
        let active = Arc::new(Mutex::new(None));
        let thread_active = Arc::clone(&active);
        let thread_stop = Arc::clone(&stop);
        let join = thread::spawn(move || {
            if let Err(error) = self.serve(&thread_stop, &thread_active) {
                eprintln!("omnifs-libkrun: attach bridge failed: {error}");
                let _ = shutdown_fd.signal();
            }
        });
        AttachBridgeThread { join, active, stop }
    }

    fn serve(self, stop: &AtomicBool, active: &Mutex<Option<ActiveRelay>>) -> Result<(), Error> {
        while !stop.load(Ordering::Acquire) {
            match self.listener.accept() {
                Ok((guest, _)) => {
                    guest.set_nonblocking(false).map_err(|error| {
                        Error::io(
                            "set accepted attach bridge stream blocking",
                            &self.path,
                            error,
                        )
                    })?;
                    match UnixStream::connect(&self.target) {
                        Ok(daemon) => Self::relay(guest, daemon, &self.path, stop, active)?,
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::ConnectionRefused
                                    | std::io::ErrorKind::NotFound
                            ) =>
                        {
                            // Closing the accepted guest leg makes this one
                            // attempt fail. The wire client retries with backoff
                            // and the next accept resolves the current target.
                        },
                        Err(error) => {
                            return Err(Error::io(
                                "connect attach bridge to daemon socket",
                                &self.target,
                                error,
                            ));
                        },
                    }
                },
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(50));
                },
                Err(error) => {
                    return Err(Error::io(
                        "accept attach bridge connection on",
                        &self.path,
                        error,
                    ));
                },
            }
        }
        Ok(())
    }

    fn relay(
        guest: UnixStream,
        daemon: UnixStream,
        path: &Path,
        stop: &AtomicBool,
        active: &Mutex<Option<ActiveRelay>>,
    ) -> Result<(), Error> {
        use std::net::Shutdown;

        let mut guest_reader = guest
            .try_clone()
            .map_err(|error| Error::io("clone guest attach bridge stream for", path, error))?;
        let guest_shutdown = guest
            .try_clone()
            .map_err(|error| Error::io("clone guest attach bridge shutdown for", path, error))?;
        let mut guest_writer = guest;

        let mut daemon_reader = daemon
            .try_clone()
            .map_err(|error| Error::io("clone daemon attach bridge stream for", path, error))?;
        let daemon_shutdown = daemon
            .try_clone()
            .map_err(|error| Error::io("clone daemon attach bridge shutdown for", path, error))?;
        let mut daemon_writer = daemon;

        {
            let mut active = active
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if stop.load(Ordering::Acquire) {
                return Ok(());
            }
            *active = Some(ActiveRelay {
                guest: guest_shutdown.try_clone().map_err(|error| {
                    Error::io("track guest attach bridge stream for", path, error)
                })?,
                daemon: daemon_shutdown.try_clone().map_err(|error| {
                    Error::io("track daemon attach bridge stream for", path, error)
                })?,
            });
        }

        let (done_tx, done_rx) = mpsc::sync_channel(2);
        thread::scope(|scope| {
            let guest_to_daemon = done_tx.clone();
            scope.spawn(move || {
                let _ = std::io::copy(&mut guest_reader, &mut daemon_writer);
                let _ = guest_to_daemon.send(());
            });
            scope.spawn(move || {
                let _ = std::io::copy(&mut daemon_reader, &mut guest_writer);
                let _ = done_tx.send(());
            });

            let _ = done_rx.recv();
            let _ = guest_shutdown.shutdown(Shutdown::Both);
            let _ = daemon_shutdown.shutdown(Shutdown::Both);
        });
        *active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        Ok(())
    }
}

impl Drop for AttachBridge {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

struct Control {
    path: PathBuf,
    listener: UnixListener,
    record: HelperRecord,
}

impl Control {
    fn bind(
        path: &Path,
        filesystem: &omnifs_core::ResourceName,
        spec: &omnifs_core::FilesystemSpec,
        instance_id: &str,
    ) -> Result<Self, Error> {
        let listener = UnixListener::bind(path)
            .map_err(|error| Error::io("bind control socket", path, error))?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| Error::io("restrict control socket", path, error))?;
        listener
            .set_nonblocking(true)
            .map_err(|error| Error::io("set control socket nonblocking", path, error))?;
        Ok(Self {
            path: path.to_path_buf(),
            listener,
            record: HelperRecord::new(
                std::process::id(),
                filesystem.clone(),
                spec.clone(),
                instance_id,
            )?,
        })
    }

    fn spawn(
        self,
        shutdown_fd: ShutdownFd,
        stop: Arc<AtomicBool>,
    ) -> thread::JoinHandle<Result<(), Error>> {
        thread::spawn(move || self.serve(shutdown_fd, &stop))
    }

    fn serve(self, shutdown_fd: ShutdownFd, stop: &AtomicBool) -> Result<(), Error> {
        while !stop.load(Ordering::Acquire) {
            match self.listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(1)))
                        .map_err(|error| {
                            Error::io("set control request timeout for", &self.path, error)
                        })?;
                    stream
                        .set_write_timeout(Some(Duration::from_secs(1)))
                        .map_err(|error| {
                            Error::io("set control reply timeout for", &self.path, error)
                        })?;
                    let mut request = String::new();
                    let read = std::io::BufReader::new(&mut stream)
                        .take(CONTROL_MAX_LINE_BYTES)
                        .read_line(&mut request);
                    let Ok(read) = read else {
                        let error = read.unwrap_err();
                        eprintln!(
                            "omnifs-libkrun: invalid control request on {}: {error}",
                            self.path.display()
                        );
                        continue;
                    };
                    if read == 0
                        || !request.ends_with('\n')
                        || read as u64 == CONTROL_MAX_LINE_BYTES
                    {
                        eprintln!(
                            "omnifs-libkrun: rejected incomplete or oversized control request on {}",
                            self.path.display()
                        );
                        continue;
                    }
                    let expected_ping = format!(
                        "ping {} {}\n",
                        self.record.filesystem, self.record.instance_id
                    );
                    let expected_shutdown = format!(
                        "shutdown {} {}\n",
                        self.record.filesystem, self.record.instance_id
                    );
                    let reply = if request == expected_ping {
                        format!(
                            "pong {} {} {}\n",
                            self.record.pid, self.record.filesystem, self.record.instance_id
                        )
                    } else if request == expected_shutdown {
                        format!(
                            "ok {} {} {}\n",
                            self.record.pid, self.record.filesystem, self.record.instance_id
                        )
                    } else {
                        eprintln!(
                            "omnifs-libkrun: rejected unknown or mismatched control request on {}",
                            self.path.display()
                        );
                        continue;
                    };
                    stream
                        .write_all(reply.as_bytes())
                        .map_err(|error| Error::io("write control reply to", &self.path, error))?;
                    stream
                        .flush()
                        .map_err(|error| Error::io("flush control reply to", &self.path, error))?;
                    if request == expected_shutdown {
                        shutdown_fd.signal()?;
                    }
                },
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(50));
                },
                Err(error) => {
                    return Err(Error::io("accept control request on", &self.path, error));
                },
            }
        }
        Ok(())
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

struct PublishedPid {
    pid_file: PathBuf,
    record: HelperRecord,
}

impl PublishedPid {
    fn publish(
        pid_file: &Path,
        filesystem: &omnifs_core::ResourceName,
        spec: &omnifs_core::FilesystemSpec,
        instance_id: &str,
    ) -> Result<Self, Error> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(pid_file)
            .map_err(|error| Error::io("create helper record", pid_file, error))?;
        let record = HelperRecord::new(
            std::process::id(),
            filesystem.clone(),
            spec.clone(),
            instance_id,
        )?;
        let mut bytes = serde_json::to_vec(&record)
            .map_err(|error| Error::Control(format!("serialize helper record: {error}")))?;
        bytes.push(b'\n');
        file.write_all(&bytes)
            .map_err(|error| Error::io("write helper record", pid_file, error))?;
        file.sync_all()
            .map_err(|error| Error::io("sync helper record", pid_file, error))?;
        Ok(Self {
            pid_file: pid_file.to_path_buf(),
            record,
        })
    }
}

impl Drop for PublishedPid {
    fn drop(&mut self) {
        if HelperRecord::read(&self.pid_file).ok().flatten().as_ref() == Some(&self.record) {
            let _ = std::fs::remove_file(&self.pid_file);
        }
    }
}

impl Config {
    fn validate_runtime_inputs(&self) -> Result<(), Error> {
        self.validate()?;
        for (name, path) in [
            ("root disk", &self.root_disk),
            ("seed disk", &self.seed_disk),
            ("packaged libkrun dylib", &self.library),
            ("packaged firmware", &self.firmware),
        ] {
            if !path.is_file() {
                return Err(Error::Config(format!(
                    "{name} is missing: {}",
                    path.display()
                )));
            }
        }
        if self.pid_file.exists() {
            return Err(Error::Config(format!(
                "helper record already exists: {}",
                self.pid_file.display()
            )));
        }
        if self.control_socket.exists() {
            return Err(Error::Config(format!(
                "control socket already exists: {}",
                self.control_socket.display()
            )));
        }
        if self.attach_bridge_socket.exists() {
            return Err(Error::Config(format!(
                "attach bridge socket already exists: {}",
                self.attach_bridge_socket.display()
            )));
        }
        Ok(())
    }

    fn open_diagnostic_log(&self) -> Result<File, Error> {
        OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(&self.diagnostic_log)
            .map_err(|error| Error::io("open diagnostic log", &self.diagnostic_log, error))
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::os::fd::RawFd;
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::Mutex;

    use super::*;
    use crate::{ATTACH_BRIDGE_SOCKET_NAME, Installation, READY_SOCKET_NAME, SSH_SOCKET_NAME};

    #[derive(Debug, PartialEq, Eq)]
    enum Step {
        Feature(Feature),
        Create,
        Firmware(PathBuf),
        Vm(u8, u32),
        Disk(Disk, PathBuf),
        DisableImplicitVsock,
        AddVsock,
        Port(u32, PathBuf, PortDirection),
        Console(PathBuf),
        ShutdownFd,
        Free,
    }

    struct FakeApi {
        steps: Mutex<Vec<Step>>,
        gpu: bool,
    }

    impl FakeApi {
        fn new(gpu: bool) -> Self {
            Self {
                steps: Mutex::new(Vec::new()),
                gpu,
            }
        }

        fn push(&self, step: Step) {
            self.steps.lock().unwrap().push(step);
        }
    }

    impl Api for FakeApi {
        fn init_log(&self, _target: RawFd) -> Result<(), Error> {
            Ok(())
        }

        fn has_feature(&self, feature: Feature) -> Result<bool, Error> {
            self.push(Step::Feature(feature));
            Ok(match feature {
                Feature::Block | Feature::Efi => true,
                Feature::Gpu => self.gpu,
            })
        }

        fn create_context(&self) -> Result<u32, Error> {
            self.push(Step::Create);
            Ok(7)
        }

        fn free_context(&self, _context: u32) {
            self.push(Step::Free);
        }

        fn set_firmware(&self, _context: u32, path: &Path) -> Result<(), Error> {
            self.push(Step::Firmware(path.to_path_buf()));
            Ok(())
        }

        fn set_vm_config(&self, _context: u32, vcpus: u8, memory_mib: u32) -> Result<(), Error> {
            self.push(Step::Vm(vcpus, memory_mib));
            Ok(())
        }

        fn add_disk(&self, _context: u32, disk: Disk, path: &Path) -> Result<(), Error> {
            self.push(Step::Disk(disk, path.to_path_buf()));
            Ok(())
        }

        fn disable_implicit_vsock(&self, _context: u32) -> Result<(), Error> {
            self.push(Step::DisableImplicitVsock);
            Ok(())
        }

        fn add_vsock(&self, _context: u32) -> Result<(), Error> {
            self.push(Step::AddVsock);
            Ok(())
        }

        fn add_vsock_port(
            &self,
            _context: u32,
            port: u32,
            path: &Path,
            direction: PortDirection,
        ) -> Result<(), Error> {
            self.push(Step::Port(port, path.to_path_buf(), direction));
            Ok(())
        }

        fn set_console_output(&self, _context: u32, path: &Path) -> Result<(), Error> {
            self.push(Step::Console(path.to_path_buf()));
            Ok(())
        }

        fn shutdown_fd(&self, _context: u32) -> Result<ShutdownFd, Error> {
            self.push(Step::ShutdownFd);
            Ok(ShutdownFd::from_raw(9))
        }

        fn start_enter(&self, _context: u32) -> i32 {
            -libc::EINVAL
        }
    }

    fn config() -> Config {
        let install = Installation::for_executable("/opt/omnifs/omnifs").unwrap();
        let filesystem = omnifs_core::ResourceName::new("demo").unwrap();
        let spec = omnifs_core::FilesystemSpec::new(
            omnifs_core::FilesystemProtocol::Fuse,
            omnifs_core::FilesystemRuntime::Libkrun,
            omnifs_core::FILESYSTEM_GUEST_LOCATION.into(),
            None,
            Some("guest.raw".into()),
        )
        .unwrap();
        Config::omnifs(
            "/tmp/omnifs/libkrun",
            "/tmp/omnifs/attach.sock",
            filesystem,
            spec,
            "0123456789abcdef0123456789abcdef",
            &install,
        )
        .unwrap()
    }

    #[test]
    fn fixed_vm_configuration_calls_the_exact_non_network_api_sequence() {
        let api = FakeApi::new(false);
        let config = config();
        let context = Context::configure(&api, &config).unwrap();
        assert_eq!(context.shutdown_fd().unwrap(), ShutdownFd::from_raw(9));
        drop(context);

        assert_eq!(
            *api.steps.lock().unwrap(),
            vec![
                Step::Feature(Feature::Block),
                Step::Feature(Feature::Efi),
                Step::Feature(Feature::Gpu),
                Step::Create,
                Step::Firmware(PathBuf::from(
                    "/opt/omnifs/libexec/omnifs/KRUN_EFI.silent.fd"
                )),
                Step::Vm(2, 2048),
                Step::Disk(Disk::Root, PathBuf::from("/tmp/omnifs/libkrun/root.raw")),
                Step::Disk(Disk::Seed, PathBuf::from("/tmp/omnifs/libkrun/seed.iso")),
                Step::DisableImplicitVsock,
                Step::AddVsock,
                Step::Port(
                    1024,
                    PathBuf::from("/tmp/omnifs/libkrun").join(ATTACH_BRIDGE_SOCKET_NAME),
                    PortDirection::GuestConnects
                ),
                Step::Port(
                    1025,
                    PathBuf::from("/tmp/omnifs/libkrun").join(READY_SOCKET_NAME),
                    PortDirection::GuestConnects
                ),
                Step::Port(
                    22,
                    PathBuf::from("/tmp/omnifs/libkrun").join(SSH_SOCKET_NAME),
                    PortDirection::HostConnects
                ),
                Step::Console(PathBuf::from("/tmp/omnifs/libkrun/serial.log")),
                Step::ShutdownFd,
                Step::Free,
            ]
        );
    }

    #[test]
    fn gpu_enabled_runtime_is_rejected_before_context_creation() {
        let api = FakeApi::new(true);
        let Err(error) = Context::configure(&api, &config()) else {
            panic!("GPU-enabled libkrun should be rejected");
        };
        assert!(error.to_string().contains("GPU"));
        assert_eq!(
            *api.steps.lock().unwrap(),
            vec![
                Step::Feature(Feature::Block),
                Step::Feature(Feature::Efi),
                Step::Feature(Feature::Gpu),
            ]
        );
    }

    #[test]
    fn attach_bridge_closes_a_dead_daemon_leg_and_accepts_the_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let target_path = temp.path().join("daemon.sock");
        let bridge_path = temp.path().join(ATTACH_BRIDGE_SOCKET_NAME);
        let target = UnixListener::bind(&target_path).unwrap();
        let bridge = AttachBridge::bind(&bridge_path, &target_path).unwrap();
        assert_eq!(
            std::fs::metadata(&bridge_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        let stop = Arc::new(AtomicBool::new(false));
        let bridge_stop = Arc::clone(&stop);
        let active = Arc::new(Mutex::new(None));
        let bridge_active = Arc::clone(&active);
        let bridge_thread = thread::spawn(move || bridge.serve(&bridge_stop, &bridge_active));

        let mut guest = UnixStream::connect(&bridge_path).unwrap();
        let (mut daemon, _) = target.accept().unwrap();
        guest.write_all(b"first").unwrap();
        let mut request = [0_u8; 5];
        daemon.read_exact(&mut request).unwrap();
        assert_eq!(&request, b"first");
        daemon.write_all(b"served").unwrap();
        let mut response = [0_u8; 6];
        guest.read_exact(&mut response).unwrap();
        assert_eq!(&response, b"served");

        drop(daemon);
        guest
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        assert_eq!(guest.read(&mut [0_u8; 1]).unwrap(), 0);
        drop(guest);
        drop(target);
        std::fs::remove_file(&target_path).unwrap();

        let replacement = UnixListener::bind(&target_path).unwrap();
        let mut guest = UnixStream::connect(&bridge_path).unwrap();
        let (mut daemon, _) = replacement.accept().unwrap();
        guest.write_all(b"again").unwrap();
        let mut request = [0_u8; 5];
        daemon.read_exact(&mut request).unwrap();
        assert_eq!(&request, b"again");

        drop(guest);
        drop(daemon);
        stop.store(true, Ordering::Release);
        bridge_thread.join().unwrap().unwrap();
        assert!(!bridge_path.exists());
    }

    #[test]
    fn attach_bridge_stop_interrupts_an_active_relay() {
        let temp = tempfile::tempdir().unwrap();
        let target_path = temp.path().join("daemon.sock");
        let bridge_path = temp.path().join(ATTACH_BRIDGE_SOCKET_NAME);
        let target = UnixListener::bind(&target_path).unwrap();
        let bridge = AttachBridge::bind(&bridge_path, &target_path).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let (_signal_reader, signal_writer) = UnixStream::pair().unwrap();
        let bridge_thread = bridge.spawn(
            ShutdownFd::from_raw(signal_writer.as_raw_fd()),
            Arc::clone(&stop),
        );

        let mut guest = UnixStream::connect(&bridge_path).unwrap();
        let (mut daemon, _) = target.accept().unwrap();
        guest.write_all(b"active").unwrap();
        let mut request = [0_u8; 6];
        daemon.read_exact(&mut request).unwrap();
        assert_eq!(&request, b"active");
        guest
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();

        bridge_thread.stop();
        bridge_thread.join();

        assert_eq!(guest.read(&mut [0_u8; 1]).unwrap(), 0);
        assert!(!bridge_path.exists());
    }

    #[test]
    fn helper_control_proves_identity_before_shutdown() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(crate::CONTROL_SOCKET_NAME);
        let instance = "0123456789abcdef0123456789abcdef";
        let filesystem = omnifs_core::ResourceName::new("demo").unwrap();
        let spec = omnifs_core::FilesystemSpec::new(
            omnifs_core::FilesystemProtocol::Fuse,
            omnifs_core::FilesystemRuntime::Libkrun,
            omnifs_core::FILESYSTEM_GUEST_LOCATION.into(),
            None,
            Some("guest.raw".into()),
        )
        .unwrap();
        let control = Control::bind(&path, &filesystem, &spec, instance).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let (mut signal_reader, signal_writer) = UnixStream::pair().unwrap();
        let thread = control.spawn(
            ShutdownFd::from_raw(signal_writer.as_raw_fd()),
            Arc::clone(&stop),
        );
        let client = crate::ControlSocket::new(path).unwrap();
        let record = client.ping(&filesystem, &spec, instance).unwrap();
        assert_eq!(record.pid, std::process::id());
        assert_eq!(record.spec, spec);
        assert_eq!(record.instance_id, instance);

        client.request_shutdown(&record).unwrap();
        let mut signal = [0_u8; 8];
        signal_reader.read_exact(&mut signal).unwrap();

        stop.store(true, Ordering::Release);
        thread.join().unwrap().unwrap();
        drop(signal_writer);
    }

    #[test]
    fn published_pid_drop_never_removes_a_replacement_record() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(crate::PID_FILE_NAME);
        let filesystem = omnifs_core::ResourceName::new("demo").unwrap();
        let spec = omnifs_core::FilesystemSpec::new(
            omnifs_core::FilesystemProtocol::Fuse,
            omnifs_core::FilesystemRuntime::Libkrun,
            omnifs_core::FILESYSTEM_GUEST_LOCATION.into(),
            None,
            Some("guest.raw".into()),
        )
        .unwrap();
        let published = PublishedPid::publish(
            &path,
            &filesystem,
            &spec,
            "0123456789abcdef0123456789abcdef",
        )
        .unwrap();
        let replacement = HelperRecord::new(
            std::process::id(),
            filesystem,
            spec,
            "fedcba9876543210fedcba9876543210",
        )
        .unwrap();
        std::fs::write(&path, serde_json::to_vec(&replacement).unwrap()).unwrap();

        drop(published);

        assert_eq!(HelperRecord::read(&path).unwrap(), Some(replacement));
    }
}
