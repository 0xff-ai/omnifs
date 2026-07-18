//! Private, fixed-purpose libkrun launcher used by the Omnifs CLI.
//!
//! The public Rust surface exists only to keep helper command construction and
//! parsing under one owner. The executable accepts no generic devices,
//! networking, GPU settings, library search paths, or libkrun flags.

mod api;
mod launch;

use std::ffi::{OsStr, OsString};
use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

pub use launch::run;

pub const ROOT_DISK_NAME: &str = "root.raw";
pub const SEED_DISK_NAME: &str = "seed.iso";
pub const SERIAL_LOG_NAME: &str = "serial.log";
pub const DIAGNOSTIC_LOG_NAME: &str = "helper.log";
pub const PID_FILE_NAME: &str = "libkrun.pid";
pub const CONTROL_SOCKET_NAME: &str = "control.sock";
pub const ATTACH_BRIDGE_SOCKET_NAME: &str = "attach.sock";
pub const READY_SOCKET_NAME: &str = "ready.sock";
pub const SSH_SOCKET_NAME: &str = "ssh.sock";

pub const LIBKRUN_RELATIVE_PATH: &str = "libexec/omnifs/libkrun.1.dylib";
pub const FIRMWARE_RELATIVE_PATH: &str = "libexec/omnifs/KRUN_EFI.silent.fd";
pub const MANIFEST_RELATIVE_PATH: &str = "libexec/omnifs/runtime-manifest.json";

const HELPER_NAME: &str = "omnifs-libkrun";
const ATTACH_PORT: u32 = 1024;
const READINESS_PORT: u32 = 1025;
const SSH_PORT: u32 = 22;
const VCPUS: u8 = 2;
const MEMORY_MIB: u32 = 2048;
const SHUTDOWN_REQUEST: &[u8] = b"shutdown\n";
const SHUTDOWN_REPLY: &[u8] = b"ok\n";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("the libkrun runtime is supported only on macOS arm64")]
    UnsupportedPlatform,
    #[error("resolve the current executable: {0}")]
    CurrentExecutable(#[source] std::io::Error),
    #[error("the current executable has no parent directory: {0}")]
    ExecutableHasNoParent(PathBuf),
    #[error("the packaged libkrun helper is missing or is not executable: {0}")]
    MissingHelper(PathBuf),
    #[error("the packaged libkrun dylib is missing: {0}")]
    MissingLibrary(PathBuf),
    #[error("the packaged libkrun firmware is missing: {0}")]
    MissingFirmware(PathBuf),
    #[error("the packaged libkrun runtime manifest is missing: {0}")]
    MissingManifest(PathBuf),
    #[error("invalid omnifs-libkrun arguments: {0}")]
    Arguments(String),
    #[error("invalid omnifs-libkrun configuration: {0}")]
    Config(String),
    #[error("path contains a NUL byte: {0}")]
    PathContainsNul(PathBuf),
    #[error("load packaged libkrun dylib {path}: {source}")]
    LoadLibrary {
        path: PathBuf,
        #[source]
        source: libloading::Error,
    },
    #[error("packaged libkrun dylib {path} is missing required symbol `{symbol}`: {source}")]
    MissingSymbol {
        path: PathBuf,
        symbol: &'static str,
        #[source]
        source: libloading::Error,
    },
    #[error("libkrun call `{function}` failed with errno {}", -*code)]
    Call { function: &'static str, code: i32 },
    #[error("libkrun call `{function}` returned unexpected value {value}")]
    UnexpectedReturn { function: &'static str, value: i32 },
    #[error("{operation} {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("the libkrun control thread failed: {0}")]
    Control(String),
}

impl Error {
    fn io(operation: &'static str, path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            operation,
            path: path.into(),
            source,
        }
    }
}

/// One installed Darwin arm64 payload rooted beside the `omnifs` executable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Installation {
    helper: PathBuf,
    library: PathBuf,
    firmware: PathBuf,
    manifest: PathBuf,
}

impl Installation {
    pub fn current() -> Result<Self, Error> {
        let executable = std::env::current_exe().map_err(Error::CurrentExecutable)?;
        Self::for_executable(executable)
    }

    pub fn for_executable(executable: impl AsRef<Path>) -> Result<Self, Error> {
        let executable = executable.as_ref();
        let root = executable
            .parent()
            .ok_or_else(|| Error::ExecutableHasNoParent(executable.to_path_buf()))?;
        Ok(Self {
            helper: root.join(HELPER_NAME),
            library: root.join(LIBKRUN_RELATIVE_PATH),
            firmware: root.join(FIRMWARE_RELATIVE_PATH),
            manifest: root.join(MANIFEST_RELATIVE_PATH),
        })
    }

    pub fn probe(&self) -> Result<(), Error> {
        if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            return Err(Error::UnsupportedPlatform);
        }
        if !is_executable_file(&self.helper) {
            return Err(Error::MissingHelper(self.helper.clone()));
        }
        if !self.library.is_file() {
            return Err(Error::MissingLibrary(self.library.clone()));
        }
        if !self.firmware.is_file() {
            return Err(Error::MissingFirmware(self.firmware.clone()));
        }
        if !self.manifest.is_file() {
            return Err(Error::MissingManifest(self.manifest.clone()));
        }
        Ok(())
    }

    pub fn helper(&self) -> &Path {
        &self.helper
    }
}

fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;

    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// The only VM shape the helper accepts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    root_disk: PathBuf,
    seed_disk: PathBuf,
    serial_log: PathBuf,
    diagnostic_log: PathBuf,
    pid_file: PathBuf,
    control_socket: PathBuf,
    attach_socket: PathBuf,
    attach_bridge_socket: PathBuf,
    readiness_socket: PathBuf,
    ssh_socket: PathBuf,
    library: PathBuf,
    firmware: PathBuf,
    attach_port: u32,
    readiness_port: u32,
    ssh_port: u32,
    vcpus: u8,
    memory_mib: u32,
}

impl Config {
    pub fn omnifs(
        state_dir: impl AsRef<Path>,
        attach_socket: impl AsRef<Path>,
        installation: &Installation,
    ) -> Result<Self, Error> {
        let state_dir = state_dir.as_ref();
        let config = Self {
            root_disk: state_dir.join(ROOT_DISK_NAME),
            seed_disk: state_dir.join(SEED_DISK_NAME),
            serial_log: state_dir.join(SERIAL_LOG_NAME),
            diagnostic_log: state_dir.join(DIAGNOSTIC_LOG_NAME),
            pid_file: state_dir.join(PID_FILE_NAME),
            control_socket: state_dir.join(CONTROL_SOCKET_NAME),
            attach_socket: attach_socket.as_ref().to_path_buf(),
            attach_bridge_socket: state_dir.join(ATTACH_BRIDGE_SOCKET_NAME),
            readiness_socket: state_dir.join(READY_SOCKET_NAME),
            ssh_socket: state_dir.join(SSH_SOCKET_NAME),
            library: installation.library.clone(),
            firmware: installation.firmware.clone(),
            attach_port: ATTACH_PORT,
            readiness_port: READINESS_PORT,
            ssh_port: SSH_PORT,
            vcpus: VCPUS,
            memory_mib: MEMORY_MIB,
        };
        config.validate()?;
        Ok(config)
    }

    pub fn apply_to(&self, command: &mut Command) {
        for (flag, value) in self.arguments() {
            command.arg(flag).arg(value);
        }
    }

    pub fn parse(arguments: impl IntoIterator<Item = OsString>) -> Result<Self, Error> {
        Parser::new(arguments.into_iter()).parse()
    }

    pub fn diagnostic_log(&self) -> &Path {
        &self.diagnostic_log
    }

    pub fn pid_file(&self) -> &Path {
        &self.pid_file
    }

    pub fn control_socket(&self) -> &Path {
        &self.control_socket
    }

    pub fn control(&self) -> ControlSocket {
        ControlSocket::new_unchecked(self.control_socket.clone())
    }

    fn arguments(&self) -> [(&'static str, OsString); 16] {
        [
            ("--root-disk", self.root_disk.clone().into_os_string()),
            ("--seed-disk", self.seed_disk.clone().into_os_string()),
            ("--serial-log", self.serial_log.clone().into_os_string()),
            (
                "--diagnostic-log",
                self.diagnostic_log.clone().into_os_string(),
            ),
            ("--pid-file", self.pid_file.clone().into_os_string()),
            (
                "--control-socket",
                self.control_socket.clone().into_os_string(),
            ),
            (
                "--attach-socket",
                self.attach_socket.clone().into_os_string(),
            ),
            (
                "--attach-bridge-socket",
                self.attach_bridge_socket.clone().into_os_string(),
            ),
            (
                "--readiness-socket",
                self.readiness_socket.clone().into_os_string(),
            ),
            ("--ssh-socket", self.ssh_socket.clone().into_os_string()),
            ("--libkrun", self.library.clone().into_os_string()),
            ("--firmware", self.firmware.clone().into_os_string()),
            ("--attach-port", self.attach_port.to_string().into()),
            ("--readiness-port", self.readiness_port.to_string().into()),
            ("--ssh-port", self.ssh_port.to_string().into()),
            (
                "--resources",
                format!("{}:{}", self.vcpus, self.memory_mib).into(),
            ),
        ]
    }

    fn validate(&self) -> Result<(), Error> {
        for (name, path) in [
            ("root disk", &self.root_disk),
            ("seed disk", &self.seed_disk),
            ("serial log", &self.serial_log),
            ("diagnostic log", &self.diagnostic_log),
            ("pid file", &self.pid_file),
            ("control socket", &self.control_socket),
            ("attach socket", &self.attach_socket),
            ("attach bridge socket", &self.attach_bridge_socket),
            ("readiness socket", &self.readiness_socket),
            ("ssh socket", &self.ssh_socket),
            ("libkrun dylib", &self.library),
            ("firmware", &self.firmware),
        ] {
            if !path.is_absolute() {
                return Err(Error::Config(format!(
                    "{name} path must be absolute: {}",
                    path.display()
                )));
            }
        }
        if (self.attach_port, self.readiness_port, self.ssh_port)
            != (ATTACH_PORT, READINESS_PORT, SSH_PORT)
        {
            return Err(Error::Config(format!(
                "vsock ports must be attach={ATTACH_PORT}, readiness={READINESS_PORT}, ssh={SSH_PORT}"
            )));
        }
        if (self.vcpus, self.memory_mib) != (VCPUS, MEMORY_MIB) {
            return Err(Error::Config(format!(
                "resources must be {VCPUS} vCPUs and {MEMORY_MIB} MiB"
            )));
        }
        Ok(())
    }
}

/// The helper's fixed, filesystem-authenticated shutdown endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlSocket {
    path: PathBuf,
}

impl ControlSocket {
    pub fn new(path: impl Into<PathBuf>) -> Result<Self, Error> {
        let path = path.into();
        if !path.is_absolute() {
            return Err(Error::Config(format!(
                "control socket path must be absolute: {}",
                path.display()
            )));
        }
        Ok(Self::new_unchecked(path))
    }

    fn new_unchecked(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn request_shutdown(&self) -> Result<(), Error> {
        let mut stream = UnixStream::connect(&self.path)
            .map_err(|error| Error::io("connect to control socket", &self.path, error))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .map_err(|error| Error::io("set control socket read timeout for", &self.path, error))?;
        stream
            .set_write_timeout(Some(Duration::from_secs(1)))
            .map_err(|error| {
                Error::io("set control socket write timeout for", &self.path, error)
            })?;
        stream
            .write_all(SHUTDOWN_REQUEST)
            .map_err(|error| Error::io("write shutdown request to", &self.path, error))?;
        let mut reply = [0_u8; SHUTDOWN_REPLY.len()];
        stream
            .read_exact(&mut reply)
            .map_err(|error| Error::io("read shutdown reply from", &self.path, error))?;
        if reply != SHUTDOWN_REPLY {
            return Err(Error::Control(format!(
                "unexpected reply from {}",
                self.path.display()
            )));
        }
        Ok(())
    }
}

struct Parser<I> {
    arguments: I,
    config: Parsed,
}

#[derive(Default)]
struct Parsed {
    root_disk: Option<PathBuf>,
    seed_disk: Option<PathBuf>,
    serial_log: Option<PathBuf>,
    diagnostic_log: Option<PathBuf>,
    pid_file: Option<PathBuf>,
    control_socket: Option<PathBuf>,
    attach_socket: Option<PathBuf>,
    attach_bridge_socket: Option<PathBuf>,
    readiness_socket: Option<PathBuf>,
    ssh_socket: Option<PathBuf>,
    library: Option<PathBuf>,
    firmware: Option<PathBuf>,
    attach_port: Option<u32>,
    readiness_port: Option<u32>,
    ssh_port: Option<u32>,
    resources: Option<(u8, u32)>,
}

impl<I: Iterator<Item = OsString>> Parser<I> {
    fn new(arguments: I) -> Self {
        Self {
            arguments,
            config: Parsed::default(),
        }
    }

    fn parse(mut self) -> Result<Config, Error> {
        while let Some(flag) = self.arguments.next() {
            let value = self.arguments.next().ok_or_else(|| {
                Error::Arguments(format!("{} requires a value", flag.to_string_lossy()))
            })?;
            match flag.to_str() {
                Some("--root-disk") => Self::path(&mut self.config.root_disk, &flag, value)?,
                Some("--seed-disk") => Self::path(&mut self.config.seed_disk, &flag, value)?,
                Some("--serial-log") => Self::path(&mut self.config.serial_log, &flag, value)?,
                Some("--diagnostic-log") => {
                    Self::path(&mut self.config.diagnostic_log, &flag, value)?;
                },
                Some("--pid-file") => Self::path(&mut self.config.pid_file, &flag, value)?,
                Some("--control-socket") => {
                    Self::path(&mut self.config.control_socket, &flag, value)?;
                },
                Some("--attach-socket") => {
                    Self::path(&mut self.config.attach_socket, &flag, value)?;
                },
                Some("--attach-bridge-socket") => {
                    Self::path(&mut self.config.attach_bridge_socket, &flag, value)?;
                },
                Some("--readiness-socket") => {
                    Self::path(&mut self.config.readiness_socket, &flag, value)?;
                },
                Some("--ssh-socket") => Self::path(&mut self.config.ssh_socket, &flag, value)?,
                Some("--libkrun") => Self::path(&mut self.config.library, &flag, value)?,
                Some("--firmware") => Self::path(&mut self.config.firmware, &flag, value)?,
                Some("--attach-port") => {
                    Self::number(&mut self.config.attach_port, &flag, &value)?;
                },
                Some("--readiness-port") => {
                    Self::number(&mut self.config.readiness_port, &flag, &value)?;
                },
                Some("--ssh-port") => Self::number(&mut self.config.ssh_port, &flag, &value)?,
                Some("--resources") => {
                    if self.config.resources.is_some() {
                        return Err(Error::Arguments(
                            "`--resources` was supplied more than once".to_owned(),
                        ));
                    }
                    let value = value.to_str().ok_or_else(|| {
                        Error::Arguments("`--resources` must be valid UTF-8".to_owned())
                    })?;
                    let (vcpus, memory) = value.split_once(':').ok_or_else(|| {
                        Error::Arguments("`--resources` must have VCPUS:MEMORY_MIB".to_owned())
                    })?;
                    self.config.resources = Some((
                        vcpus.parse().map_err(|_| {
                            Error::Arguments("invalid vCPU count in `--resources`".to_owned())
                        })?,
                        memory.parse().map_err(|_| {
                            Error::Arguments("invalid memory in `--resources`".to_owned())
                        })?,
                    ));
                },
                _ => {
                    return Err(Error::Arguments(format!(
                        "unknown flag `{}`",
                        flag.to_string_lossy()
                    )));
                },
            }
        }
        self.config.finish()
    }

    fn path(target: &mut Option<PathBuf>, flag: &OsStr, value: OsString) -> Result<(), Error> {
        if target.replace(PathBuf::from(value)).is_some() {
            return Err(Error::Arguments(format!(
                "`{}` was supplied more than once",
                flag.to_string_lossy()
            )));
        }
        Ok(())
    }

    fn number<T: std::str::FromStr>(
        target: &mut Option<T>,
        flag: &OsStr,
        value: &OsStr,
    ) -> Result<(), Error> {
        if target.is_some() {
            return Err(Error::Arguments(format!(
                "`{}` was supplied more than once",
                flag.to_string_lossy()
            )));
        }
        let value = value.to_str().ok_or_else(|| {
            Error::Arguments(format!("`{}` must be valid UTF-8", flag.to_string_lossy()))
        })?;
        *target = Some(value.parse().map_err(|_| {
            Error::Arguments(format!(
                "invalid number for `{}`: {value}",
                flag.to_string_lossy()
            ))
        })?);
        Ok(())
    }
}

impl Parsed {
    fn finish(self) -> Result<Config, Error> {
        let required = |value: Option<PathBuf>, flag: &'static str| {
            value.ok_or_else(|| Error::Arguments(format!("missing required `{flag}`")))
        };
        let number = |value: Option<u32>, flag: &'static str| {
            value.ok_or_else(|| Error::Arguments(format!("missing required `{flag}`")))
        };
        let (vcpus, memory_mib) = self
            .resources
            .ok_or_else(|| Error::Arguments("missing required `--resources`".to_owned()))?;
        let config = Config {
            root_disk: required(self.root_disk, "--root-disk")?,
            seed_disk: required(self.seed_disk, "--seed-disk")?,
            serial_log: required(self.serial_log, "--serial-log")?,
            diagnostic_log: required(self.diagnostic_log, "--diagnostic-log")?,
            pid_file: required(self.pid_file, "--pid-file")?,
            control_socket: required(self.control_socket, "--control-socket")?,
            attach_socket: required(self.attach_socket, "--attach-socket")?,
            attach_bridge_socket: required(self.attach_bridge_socket, "--attach-bridge-socket")?,
            readiness_socket: required(self.readiness_socket, "--readiness-socket")?,
            ssh_socket: required(self.ssh_socket, "--ssh-socket")?,
            library: required(self.library, "--libkrun")?,
            firmware: required(self.firmware, "--firmware")?,
            attach_port: number(self.attach_port, "--attach-port")?,
            readiness_port: number(self.readiness_port, "--readiness-port")?,
            ssh_port: number(self.ssh_port, "--ssh-port")?,
            vcpus,
            memory_mib,
        };
        config.validate()?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (Installation, Config) {
        let install = Installation::for_executable("/opt/omnifs/omnifs").unwrap();
        let config =
            Config::omnifs("/tmp/omnifs/libkrun", "/tmp/omnifs/attach.sock", &install).unwrap();
        (install, config)
    }

    #[test]
    fn installed_layout_is_relative_to_the_omnifs_executable() {
        let (installation, _) = fixture();
        assert_eq!(installation.helper, Path::new("/opt/omnifs/omnifs-libkrun"));
        assert_eq!(
            installation.library,
            Path::new("/opt/omnifs/libexec/omnifs/libkrun.1.dylib")
        );
        assert_eq!(
            installation.firmware,
            Path::new("/opt/omnifs/libexec/omnifs/KRUN_EFI.silent.fd")
        );
    }

    #[test]
    fn command_arguments_round_trip_through_the_strict_parser() {
        let (_, config) = fixture();
        let arguments = config
            .arguments()
            .into_iter()
            .flat_map(|(flag, value)| [OsString::from(flag), value])
            .collect::<Vec<_>>();
        assert_eq!(Config::parse(arguments).unwrap(), config);
    }

    #[test]
    fn parser_rejects_a_resource_policy_change() {
        let (_, config) = fixture();
        let mut arguments = config
            .arguments()
            .into_iter()
            .flat_map(|(flag, value)| [OsString::from(flag), value])
            .collect::<Vec<_>>();
        let index = arguments
            .iter()
            .position(|argument| argument == "--resources")
            .unwrap();
        arguments[index + 1] = "4:4096".into();
        let error = Config::parse(arguments).unwrap_err();
        assert!(error.to_string().contains("2 vCPUs and 2048 MiB"));
    }
}
