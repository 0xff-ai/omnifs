//! Private, fixed-purpose libkrun launcher used by the Omnifs CLI.
//!
//! The public Rust surface exists only to keep helper command construction and
//! parsing under one owner. The executable accepts no generic devices,
//! networking, GPU settings, library search paths, or libkrun flags.

mod api;
mod launch;

use std::ffi::OsString;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use omnifs_core::{
    ATTACHMENT_GUEST_LOCATION, AttachmentProtocol, AttachmentRuntime, AttachmentSpec, ResourceName,
};
use serde::{Deserialize, Serialize};

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
const HELPER_RECORD_VERSION: u8 = 4;
const CONTROL_MAX_LINE_BYTES: u64 = 128;
const CONTROL_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HelperRecord {
    pub version: u8,
    pub pid: u32,
    pub attachment: ResourceName,
    pub spec: AttachmentSpec,
    pub instance_id: String,
}

impl HelperRecord {
    pub fn new(
        pid: u32,
        attachment: ResourceName,
        spec: AttachmentSpec,
        instance_id: impl Into<String>,
    ) -> Result<Self, Error> {
        let record = Self {
            version: HELPER_RECORD_VERSION,
            pid,
            attachment,
            spec,
            instance_id: instance_id.into(),
        };
        record.validate()?;
        Ok(record)
    }

    pub fn read(path: &Path) -> Result<Option<Self>, Error> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(Error::io("read helper record", path, error)),
        };
        let record: Self = serde_json::from_slice(&bytes).map_err(|error| {
            Error::Control(format!(
                "parse strict helper record {}: {error}",
                path.display()
            ))
        })?;
        record.validate()?;
        Ok(Some(record))
    }

    fn validate(&self) -> Result<(), Error> {
        if self.version != HELPER_RECORD_VERSION {
            return Err(Error::Control(format!(
                "unsupported helper record version {}",
                self.version
            )));
        }
        validate_instance_id(&self.instance_id)
    }
}

fn validate_instance_id(instance_id: &str) -> Result<(), Error> {
    if instance_id.len() != 32
        || !instance_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(Error::Config(
            "instance id must be exactly 32 lowercase hexadecimal characters".to_owned(),
        ));
    }
    Ok(())
}

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
    state_dir: PathBuf,
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
    attachment: ResourceName,
    spec: AttachmentSpec,
    instance_id: String,
}

impl Config {
    pub fn omnifs(
        state_dir: impl AsRef<Path>,
        attach_socket: impl AsRef<Path>,
        attachment: ResourceName,
        spec: AttachmentSpec,
        instance_id: impl Into<String>,
        installation: &Installation,
    ) -> Result<Self, Error> {
        let state_dir = state_dir.as_ref();
        let config = Self {
            state_dir: state_dir.to_path_buf(),
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
            attachment,
            spec,
            instance_id: instance_id.into(),
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
        let arguments = arguments.into_iter().collect::<Vec<_>>();
        let [
            state_flag,
            state_dir,
            attach_flag,
            attach_socket,
            name_flag,
            filesystem_id,
            protocol_flag,
            protocol,
            runtime_flag,
            runtime,
            location_flag,
            location,
            guest_image_flag,
            guest_image,
            instance_flag,
            instance_id,
        ] = arguments.as_slice()
        else {
            return Err(Error::Arguments(
                "expected `--state-dir PATH --attach-socket PATH --name NAME --protocol PROTOCOL \
                 --runtime RUNTIME --location PATH --libkrun-guest-image IMAGE --instance-id ID`"
                    .to_owned(),
            ));
        };
        if state_flag != "--state-dir"
            || attach_flag != "--attach-socket"
            || name_flag != "--name"
            || protocol_flag != "--protocol"
            || runtime_flag != "--runtime"
            || location_flag != "--location"
            || guest_image_flag != "--libkrun-guest-image"
            || instance_flag != "--instance-id"
        {
            return Err(Error::Arguments(
                "expected `--state-dir PATH --attach-socket PATH --name NAME --protocol PROTOCOL \
                 --runtime RUNTIME --location PATH --libkrun-guest-image IMAGE --instance-id ID`"
                    .to_owned(),
            ));
        }
        Self::omnifs(
            PathBuf::from(state_dir),
            PathBuf::from(attach_socket),
            filesystem_id
                .to_string_lossy()
                .parse()
                .map_err(|error| Error::Arguments(format!("invalid Attachment name: {error}")))?,
            AttachmentSpec::new(
                protocol.to_string_lossy().parse().map_err(|error| {
                    Error::Arguments(format!("invalid Attachment protocol: {error}"))
                })?,
                runtime.to_string_lossy().parse().map_err(|error| {
                    Error::Arguments(format!("invalid Attachment runtime: {error}"))
                })?,
                PathBuf::from(location),
                None,
                (!guest_image.is_empty()).then(|| guest_image.to_string_lossy().into_owned()),
            )
            .map_err(|error| Error::Arguments(format!("invalid Attachment spec: {error}")))?,
            instance_id.to_string_lossy(),
            &Installation::current()?,
        )
    }

    pub fn diagnostic_log(&self) -> &Path {
        &self.diagnostic_log
    }

    fn arguments(&self) -> [(&'static str, OsString); 8] {
        [
            ("--state-dir", self.state_dir.clone().into_os_string()),
            (
                "--attach-socket",
                self.attach_socket.clone().into_os_string(),
            ),
            ("--name", OsString::from(self.attachment.as_str())),
            ("--protocol", OsString::from(self.spec.protocol().as_str())),
            ("--runtime", OsString::from(self.spec.runtime().as_str())),
            ("--location", self.spec.location().as_os_str().to_owned()),
            (
                "--libkrun-guest-image",
                OsString::from(self.spec.libkrun_guest_image().unwrap_or_default()),
            ),
            ("--instance-id", OsString::from(&self.instance_id)),
        ]
    }

    fn validate(&self) -> Result<(), Error> {
        for (name, path) in [
            ("state directory", &self.state_dir),
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
        if self.spec.protocol() != AttachmentProtocol::Fuse
            || self.spec.runtime() != AttachmentRuntime::Libkrun
            || self.spec.location() != Path::new(ATTACHMENT_GUEST_LOCATION)
        {
            return Err(Error::Config(format!(
                "libkrun requires a fuse/libkrun filesystem at {ATTACHMENT_GUEST_LOCATION}"
            )));
        }
        if (self.vcpus, self.memory_mib) != (VCPUS, MEMORY_MIB) {
            return Err(Error::Config(format!(
                "resources must be {VCPUS} vCPUs and {MEMORY_MIB} MiB"
            )));
        }
        validate_instance_id(&self.instance_id)?;
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

    pub fn ping(
        &self,
        expected_attachment: &ResourceName,
        expected_spec: &AttachmentSpec,
        expected_instance_id: &str,
    ) -> Result<HelperRecord, Error> {
        validate_instance_id(expected_instance_id)?;
        let reply = self.request(&format!(
            "ping {expected_attachment} {expected_instance_id}\n"
        ))?;
        let mut fields = reply.trim_end().split(' ');
        let record = match (fields.next(), fields.next(), fields.next(), fields.next()) {
            (Some("pong"), Some(pid), Some(attachment_name), Some(instance_id))
                if fields.next().is_none() =>
            {
                let attachment: ResourceName = attachment_name.parse().map_err(|error| {
                    Error::Control(format!(
                        "control Ping returned an invalid filesystem id: {error}"
                    ))
                })?;
                if &attachment != expected_attachment {
                    return Err(Error::Control(format!(
                        "control Ping identity mismatch at {}",
                        self.path.display()
                    )));
                }
                HelperRecord::new(
                    pid.parse().map_err(|_| {
                        Error::Control("control Ping returned an invalid pid".to_owned())
                    })?,
                    attachment,
                    expected_spec.clone(),
                    instance_id,
                )?
            },
            _ => {
                return Err(Error::Control(format!(
                    "unexpected Ping reply from {}",
                    self.path.display()
                )));
            },
        };
        if record.attachment != *expected_attachment
            || record.spec != *expected_spec
            || record.instance_id != expected_instance_id
        {
            return Err(Error::Control(format!(
                "control Ping identity mismatch at {}",
                self.path.display()
            )));
        }
        Ok(record)
    }

    pub fn request_shutdown(&self, expected: &HelperRecord) -> Result<(), Error> {
        expected.validate()?;
        let reply = self.request(&format!(
            "shutdown {} {}\n",
            expected.attachment, expected.instance_id
        ))?;
        let expected_reply = format!(
            "ok {} {} {}\n",
            expected.pid, expected.attachment, expected.instance_id
        );
        if reply != expected_reply {
            return Err(Error::Control(format!(
                "unexpected shutdown reply from {}",
                self.path.display()
            )));
        }
        Ok(())
    }

    fn request(&self, request: &str) -> Result<String, Error> {
        let mut stream = UnixStream::connect(&self.path)
            .map_err(|error| Error::io("connect to control socket", &self.path, error))?;
        stream
            .set_read_timeout(Some(CONTROL_TIMEOUT))
            .map_err(|error| Error::io("set control socket read timeout for", &self.path, error))?;
        stream
            .set_write_timeout(Some(CONTROL_TIMEOUT))
            .map_err(|error| {
                Error::io("set control socket write timeout for", &self.path, error)
            })?;
        stream
            .write_all(request.as_bytes())
            .map_err(|error| Error::io("write control request to", &self.path, error))?;
        stream
            .flush()
            .map_err(|error| Error::io("flush control request to", &self.path, error))?;
        let mut reply = String::new();
        let mut reader = BufReader::new(stream).take(CONTROL_MAX_LINE_BYTES);
        let read = reader
            .read_line(&mut reply)
            .map_err(|error| Error::io("read control reply from", &self.path, error))?;
        if read == 0 || !reply.ends_with('\n') || read as u64 == CONTROL_MAX_LINE_BYTES {
            return Err(Error::Control(format!(
                "invalid or oversized control reply from {}",
                self.path.display()
            )));
        }
        Ok(reply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attachment() -> ResourceName {
        "demo".parse().unwrap()
    }

    fn spec() -> AttachmentSpec {
        AttachmentSpec::new(
            AttachmentProtocol::Fuse,
            AttachmentRuntime::Libkrun,
            ATTACHMENT_GUEST_LOCATION.into(),
            None,
            Some("guest.raw".into()),
        )
        .unwrap()
    }

    fn fixture() -> (Installation, Config) {
        let install = Installation::for_executable("/opt/omnifs/omnifs").unwrap();
        let config = Config::omnifs(
            "/tmp/omnifs/libkrun",
            "/tmp/omnifs/attach.sock",
            attachment(),
            spec(),
            "0123456789abcdef0123456789abcdef",
            &install,
        )
        .unwrap();
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
    fn command_arguments_reconstruct_the_fixed_shape() {
        let (_, config) = fixture();
        let arguments = config
            .arguments()
            .into_iter()
            .flat_map(|(flag, value)| [OsString::from(flag), value])
            .collect::<Vec<_>>();
        let parsed = Config::parse(arguments).unwrap();
        assert_eq!(parsed.state_dir, config.state_dir);
        assert_eq!(parsed.attach_socket, config.attach_socket);
        assert_eq!(parsed.root_disk, config.root_disk);
        assert_eq!(parsed.attachment, config.attachment);
        assert_eq!(parsed.spec, config.spec);
        assert_eq!(parsed.attach_port, ATTACH_PORT);
        assert_eq!(parsed.vcpus, VCPUS);
    }

    #[test]
    fn parser_rejects_any_attempt_to_override_the_fixed_shape() {
        let (_, config) = fixture();
        let mut arguments = config
            .arguments()
            .into_iter()
            .flat_map(|(flag, value)| [OsString::from(flag), value])
            .collect::<Vec<_>>();
        arguments.extend(["--resources".into(), "4:4096".into()]);
        let error = Config::parse(arguments).unwrap_err();
        assert!(error.to_string().contains("expected `--state-dir PATH"));
    }

    #[test]
    fn helper_record_is_strict_and_requires_a_128_bit_instance() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PID_FILE_NAME);
        std::fs::write(
            &path,
            r#"{"version":4,"pid":42,"attachment":"demo","spec":{"protocol":"fuse","runtime":"libkrun","location":"/omnifs","docker_image":null,"libkrun_guest_image":"guest.raw"},"instance_id":"0123456789abcdef0123456789abcdef"}"#,
        )
        .unwrap();
        assert_eq!(HelperRecord::read(&path).unwrap().unwrap().pid, 42);

        std::fs::write(
            &path,
            r#"{"version":4,"pid":42,"attachment":"demo","spec":{"protocol":"fuse","runtime":"libkrun","location":"/omnifs","docker_image":null,"libkrun_guest_image":"guest.raw"},"instance_id":"0123456789abcdef0123456789abcdef","extra":true}"#,
        )
        .unwrap();
        assert!(HelperRecord::read(&path).is_err());
        assert!(HelperRecord::new(42, attachment(), spec(), "short").is_err());
    }
}
