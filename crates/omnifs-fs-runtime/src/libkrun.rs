//! The libkrun filesystem runner: a macOS microVM hosting the same
//! `omnifs-thin --protocol fuse` runner and Omnifs VFS wire protocol used by the Docker
//! container, attached to the host-native daemon's namespace over vsock
//! instead of TCP.
//!
//! State lives under `client/filesystems/runtime/<id>/libkrun/`: a persistent
//! per-filesystem ed25519 keypair (survives across launches and authenticates guest ssh access
//! independent of any one VM instance) plus per-launch artifacts (a writable
//! root disk, strict helper record, seed ISO, the helper-owned attach bridge, the readiness,
//! SSH, and control sockets, and the serial log). Every path lives under the CLI client dir,
//! never a system temp dir, so `omnifs down`/`fs detach` can find and
//! remove exactly what this runner owns. The resolved guest image is an
//! immutable base artifact and is only the source for that launch-local root.
//!
//! One explicit no-TSI vsock device bridges the guest to the host, with three
//! fixed port mappings:
//! - port 1024 (attach): guest-initiated (`,listen`) through the helper-owned
//!   bridge onto the daemon's fixed Unix attach socket. This runner never
//!   creates or removes the daemon socket.
//! - port 1025 (ready): guest-initiated (`,listen`) onto a unix socket this
//!   runner binds before spawning libkrun; the launch lease accepts one later
//!   readiness beacon on it — a
//!   `,listen` device requires the host side already listening, since libkrun
//!   dials it once per guest connection rather than the reverse.
//! - port 22 (ssh): host-initiated (`,connect`, libkrun's explicit
//!   host-to-guest mode; omitting both keywords means guest-initiated):
//!   libkrun itself creates and listens on the unix socket, relaying each
//!   accepted connection into the guest's vsock-listening dropbear
//!   (`ListenStream=vsock::22` in the guest image). `omnifs fs shell` dials it
//!   through `ssh -o ProxyCommand='socat - UNIX-CONNECT:<path>'`.
//!
//! No network or GPU configuration exists in the helper's typed launch shape.
//! The helper disables libkrun's implicit TSI vsock before adding the explicit
//! device, so the guest gets neither ordinary network egress nor TSI socket
//! hijacking.

use std::future::Future;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use omnifs_api::OMNIFS_ATTACH_ADDR_ENV;
use omnifs_core::fs;
use omnifs_libkrun::{
    ATTACH_BRIDGE_SOCKET_NAME, CONTROL_SOCKET_NAME, ControlSocket, DIAGNOSTIC_LOG_NAME,
    HelperRecord, Installation, PID_FILE_NAME, READY_SOCKET_NAME, ROOT_DISK_NAME, SEED_DISK_NAME,
    SERIAL_LOG_NAME, SSH_SOCKET_NAME,
};
use tokio::io::AsyncReadExt as _;

use crate::driver::ensure_identity_unchanged;
#[cfg(test)]
use crate::process::is_alive as process_alive;
use crate::{
    BUILD_CHANNEL, BuildChannel, Candidate, ImageRef, LaunchRequest, RuntimeAdvice, RuntimeEvent,
    RuntimeEventSink, RuntimeStage, RuntimeState, advise, err_after_rollback,
};

const SSH_KEY_NAME: &str = "id_ed25519";
const ROOT_RAW_PART_PREFIX: &str = "root.raw.part.";
const SEED_STAGING_NAME: &str = "seed-staging";

/// How long a launch waits for the guest readiness beacon. Generous relative
/// to the host and Docker drivers' launch timeouts because it covers a full
/// microVM boot, not just a process or container start.
const LAUNCH_READY_TIMEOUT: Duration = Duration::from_secs(90);
/// How long a launch waits for the just-spawned helper to publish its
/// durable record.
const HELPER_RECORD_TIMEOUT: Duration = Duration::from_secs(5);
/// Poll interval shared by every short deadline-poll loop in this module.
const HELPER_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// How long teardown waits for a directly-owned child (still attached to
/// this process) to exit on its own before escalating to `kill`.
const CHILD_EXIT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long teardown waits for a directly-owned child to exit after `kill`.
const CHILD_KILL_TIMEOUT: Duration = Duration::from_secs(3);
/// How long teardown waits for a detached (not directly owned) helper to
/// remove its own durable record after a shutdown request.
const DETACHED_EXIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Guest vsock port the daemon's attach listener is proxied onto.
const ATTACH_VSOCK_PORT: u32 = 1024;
/// Guest vsock port used by the readiness beacon in `omnifs-vfs`.
/// dials once the FUSE mount is serving.
const READY_VSOCK_PORT: u32 = 1025;

fn emit_stage(
    events: &RuntimeEventSink,
    spec: &fs::Spec,
    stage: RuntimeStage,
    lifecycle_state: RuntimeState,
) {
    events.emit(RuntimeEvent::Stage {
        stage,
        runtime: fs::Runtime::Libkrun,
        id: spec.id().clone(),
        state: lifecycle_state,
    });
}

/// A placeholder hostname for the ssh command line. `ProxyCommand` replaces
/// the transport entirely, so this name is never resolved or dialed.
const SSH_GUEST_TARGET: &str = "root@omnifs-guest";

const ENV_GUEST_IMAGE: &str = "OMNIFS_GUEST_IMAGE";
/// The `just guest-image` recipe's default output path
/// (`scripts/guest-image/build.sh`'s `OUT_DIR` default), resolved relative to
/// the current working directory. A repo-root-relative default matches every
/// other dev-only default in this crate (e.g. `omnifs-filesystem:dev`) rather
/// than trying to locate the repo from an installed binary.
const DEFAULT_GUEST_IMAGE: &str = "target/guest-image/omnifs-guest.raw";
/// Release channel default: the pinned ghcr OCI artifact tag the
/// guest-image-arm64 CI job publishes and `release`'s `promote` job retags
/// to this version (mirrors `FILESYSTEM_RELEASE_IMAGE`'s version pinning).
const GUEST_RELEASE_IMAGE: &str =
    concat!("ghcr.io/0xff-ai/omnifs-guest:", env!("CARGO_PKG_VERSION"));

const SEED_VOLUME_LABEL: &str = "OMNIFS-SEED";
const SEED_CONF_NAME: &str = "omnifs-seed.conf";

/// The exact seed keys a launch ever writes. The lockdown audit
/// ([`audit_seed_staging`]) asserts the staging dir carries exactly this set
/// before it is burned into the ISO.
const SEED_CONF_KEYS: [&str; 5] = [
    OMNIFS_ATTACH_ADDR_ENV,
    "OMNIFS_CLIENT_OWNER",
    "OMNIFS_FS_ID",
    "OMNIFS_READY_VSOCK_PORT",
    "OMNIFS_SSH_PUBKEY",
];

/// Conservative `sockaddr_un.sun_path` byte budget, mirroring
/// The daemon's `check_uds_path_length` (kept as its own copy here: the
/// CLI and daemon do not share a path-validation crate, and this check is
/// small enough that a shared abstraction would cost more than it saves).
const UDS_PATH_BYTE_LIMIT: usize = 100;

fn check_uds_path_length(path: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt as _;
    let len = path.as_os_str().as_bytes().len();
    anyhow::ensure!(
        len < UDS_PATH_BYTE_LIMIT,
        "libkrun socket path {} is {len} bytes, at or beyond the {UDS_PATH_BYTE_LIMIT}-byte \
         sockaddr_un budget (Linux allows 108, macOS 104); shorten OMNIFS_HOME or move it closer \
         to the filesystem root",
        path.display()
    );
    Ok(())
}

/// The default guest image setting for each build channel: a local path for
/// dev, the pinned ghcr tag for release. Mirrors
/// `default_filesystem_image_for`.
pub const fn default_guest_image_for(channel: BuildChannel) -> &'static str {
    match channel {
        BuildChannel::Release => GUEST_RELEASE_IMAGE,
        BuildChannel::Dev => DEFAULT_GUEST_IMAGE,
    }
}

/// `omnifs fs shell`'s libkrun dispatch calls this before building the ssh
/// command: `shell_command` itself stays pure construction (no I/O), so the
/// probe belongs at the one call site that is about to actually run it.
pub fn ensure_socat_available() -> Result<()> {
    match Command::new("socat")
        .arg("-V")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(anyhow::anyhow!(
            "socat is required to reach the libkrun guest over vsock"
        )),
        Err(error) => Err(error).context("probe for socat on PATH"),
    }
}

/// The libkrun microVM filesystem runner. Durable client state and explicit
/// teardown live here; one launch's resources live in [`LibkrunLaunchLease`].
pub struct LibkrunRunner {
    dir: PathBuf,
}

impl LibkrunRunner {
    #[must_use]
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn dir(&self) -> &Path {
        &self.dir
    }

    fn ssh_key_path(&self) -> PathBuf {
        self.dir().join(SSH_KEY_NAME)
    }

    fn ssh_pubkey_path(&self) -> PathBuf {
        self.dir().join(format!("{SSH_KEY_NAME}.pub"))
    }

    fn pidfile(&self) -> PathBuf {
        self.dir().join(PID_FILE_NAME)
    }

    fn root_raw(&self) -> PathBuf {
        self.dir().join(ROOT_DISK_NAME)
    }

    fn seed_iso(&self) -> PathBuf {
        self.dir().join(SEED_DISK_NAME)
    }

    fn seed_staging(&self) -> PathBuf {
        self.dir().join(SEED_STAGING_NAME)
    }

    fn ssh_socket(&self) -> PathBuf {
        self.dir().join(SSH_SOCKET_NAME)
    }

    fn ready_socket(&self) -> PathBuf {
        self.dir().join(READY_SOCKET_NAME)
    }

    fn control_socket(&self) -> PathBuf {
        self.dir().join(CONTROL_SOCKET_NAME)
    }

    fn attach_bridge_socket(&self) -> PathBuf {
        self.dir().join(ATTACH_BRIDGE_SOCKET_NAME)
    }

    fn serial_log(&self) -> PathBuf {
        self.dir().join(SERIAL_LOG_NAME)
    }

    fn diagnostic_log(&self) -> PathBuf {
        self.dir().join(DIAGNOSTIC_LOG_NAME)
    }

    fn has_operational_state(&self) -> bool {
        [
            self.pidfile(),
            self.root_raw(),
            self.seed_iso(),
            self.ssh_socket(),
            self.ready_socket(),
            self.control_socket(),
            self.attach_bridge_socket(),
            self.serial_log(),
            self.diagnostic_log(),
            self.seed_staging(),
        ]
        .into_iter()
        .any(|path| path.exists())
            || !self.root_raw_parts().is_empty()
    }

    fn root_raw_parts(&self) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(self.dir()) else {
            return Vec::new();
        };
        entries
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(ROOT_RAW_PART_PREFIX))
            })
            .collect()
    }

    /// Generate the per-profile ed25519 keypair if absent, returning the
    /// trimmed public key line to embed in the seed. Persistent across
    /// launches (unlike the seed, which is per-launch): it authenticates
    /// guest ssh access independent of any one VM instance.
    fn ensure_ssh_keypair(&self) -> Result<String> {
        let key = self.ssh_key_path();
        if !key.exists() {
            let status = Command::new("ssh-keygen")
                .arg("-t")
                .arg("ed25519")
                .arg("-N")
                .arg("")
                .arg("-C")
                .arg("omnifs-libkrun")
                .arg("-f")
                .arg(&key)
                .arg("-q")
                .status()
                .context("run ssh-keygen to generate the libkrun guest keypair")?;
            anyhow::ensure!(status.success(), "ssh-keygen exited with {status}");
            std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("restrict {} to 0600", key.display()))?;
        }
        let pubkey_path = self.ssh_pubkey_path();
        let pubkey = std::fs::read_to_string(&pubkey_path)
            .with_context(|| format!("read {}", pubkey_path.display()))?;
        Ok(pubkey.trim().to_string())
    }

    /// Build the per-launch seed ISO: stage `omnifs-seed.conf`, audit the
    /// staging dir against the exact expected key set, then hand it to
    /// `hdiutil makehybrid`. Array args throughout: nothing here is
    /// interpolated into a shell.
    fn write_seed_iso(
        &self,
        client_owner: omnifs_core::ClientOwnerId,
        spec: &fs::Spec,
        ssh_pubkey: &str,
    ) -> Result<()> {
        let staging = self.seed_staging();
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging)
            .with_context(|| format!("create seed staging dir {}", staging.display()))?;

        let conf_path = staging.join(SEED_CONF_NAME);
        let conf = format!(
            "{OMNIFS_ATTACH_ADDR_ENV}=vsock:{ATTACH_VSOCK_PORT}\n\
             OMNIFS_CLIENT_OWNER={client_owner}\n\
             OMNIFS_FS_ID={}\n\
             OMNIFS_READY_VSOCK_PORT={READY_VSOCK_PORT}\n\
             OMNIFS_SSH_PUBKEY={ssh_pubkey}\n",
            spec.id()
        );
        std::fs::write(&conf_path, conf)
            .with_context(|| format!("write {}", conf_path.display()))?;

        audit_seed_staging(&staging)
            .map_err(|violation| anyhow::anyhow!("refusing to burn the seed ISO: {violation}"))?;

        let out = self.seed_iso();
        let _ = std::fs::remove_file(&out);
        let output = Command::new("hdiutil")
            .arg("makehybrid")
            .arg("-iso")
            .arg("-joliet")
            .arg("-default-volume-name")
            .arg(SEED_VOLUME_LABEL)
            .arg("-o")
            .arg(&out)
            .arg(&staging)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .context("run hdiutil makehybrid to build the seed ISO")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let detail = stderr.trim();
            if detail.is_empty() {
                anyhow::bail!("hdiutil makehybrid exited with {}", output.status);
            }
            anyhow::bail!("hdiutil makehybrid exited with {}: {detail}", output.status);
        }

        let _ = std::fs::remove_dir_all(&staging);
        Ok(())
    }

    fn read_helper_record(&self) -> Result<Option<HelperRecord>> {
        HelperRecord::read(&self.pidfile()).context("read the libkrun helper record")
    }

    fn diagnostic_tail(&self) -> String {
        const MAX_BYTES: usize = 32 * 1024;

        let path = self.diagnostic_log();
        let Ok(bytes) = std::fs::read(&path) else {
            return format!("helper log: {}", path.display());
        };
        let start = bytes.len().saturating_sub(MAX_BYTES);
        let text = String::from_utf8_lossy(&bytes[start..]);
        let detail = text.trim();
        if detail.is_empty() {
            format!("helper log is empty: {}", path.display())
        } else {
            format!("helper log {}:\n{detail}", path.display())
        }
    }
}

/// Bounds the seed staging dir to exactly the expected `KEY=VALUE` lines
/// before it is burned into an ISO the guest can read: one file, the exact
/// key set, no duplicates.
fn audit_seed_staging(staging: &Path) -> Result<(), String> {
    let entries: Vec<_> = std::fs::read_dir(staging)
        .map_err(|error| format!("read seed staging dir: {error}"))?
        .collect::<Result<_, _>>()
        .map_err(|error: std::io::Error| format!("read seed staging dir entry: {error}"))?;
    if entries.len() != 1 {
        return Err(format!(
            "seed staging dir must contain exactly one file, found {}",
            entries.len()
        ));
    }
    let entry = &entries[0];
    if entry.file_name() != SEED_CONF_NAME {
        return Err(format!(
            "unexpected seed staging file `{}`",
            entry.file_name().to_string_lossy()
        ));
    }

    let contents = std::fs::read_to_string(entry.path())
        .map_err(|error| format!("read {}: {error}", entry.path().display()))?;
    let mut seen = std::collections::HashSet::new();
    for line in contents.lines() {
        let Some((key, _value)) = line.split_once('=') else {
            return Err(format!("malformed seed line (no `=`): `{line}`"));
        };
        if !SEED_CONF_KEYS.contains(&key) {
            return Err(format!("unexpected seed key `{key}`"));
        }
        if !seen.insert(key) {
            return Err(format!("duplicate seed key `{key}`"));
        }
    }
    for expected in SEED_CONF_KEYS {
        if !seen.contains(expected) {
            return Err(format!("seed is missing required key `{expected}`"));
        }
    }
    Ok(())
}

/// Resolve the configured guest image into the validated immutable base path
/// used to materialize a launch-local root disk. Release images remain owned
/// by the OCI/cache module; this function only chooses the channel-specific
/// input and validates the result.
async fn resolve_guest_image(
    configured: Option<&str>,
    guest_image_cache: &Path,
    events: RuntimeEventSink,
) -> Result<PathBuf> {
    let resolved = std::env::var(ENV_GUEST_IMAGE)
        .ok()
        .or_else(|| configured.map(str::to_owned))
        .unwrap_or_else(|| default_guest_image_for(BUILD_CHANNEL).to_string());
    let path = match BUILD_CHANNEL {
        BuildChannel::Dev => PathBuf::from(resolved),
        BuildChannel::Release => {
            crate::guest_image::ensure_guest_image(
                &ImageRef::new(resolved)?,
                guest_image_cache,
                events,
            )
            .await?
        },
    };
    if !path.is_file() {
        return Err(advise(
            anyhow::anyhow!("guest image not found at {}", path.display()),
            RuntimeAdvice::BuildGuestImage,
        ));
    }
    Ok(path)
}

/// Owns one Libkrun launch from replacement through readiness publication.
/// Every resource created after replacement is cleaned here when publication
/// fails. The attach listener, immutable guest image, and SSH key are
/// deliberately not part of this cleanup set because their owners outlive one
/// launch.
struct LibkrunLaunchLease<'a> {
    runner: &'a LibkrunRunner,
    daemon_attach_socket: PathBuf,
    guest_image: PathBuf,
    child: Option<std::process::Child>,
    ready_listener: Option<tokio::net::UnixListener>,
    instance_id: Option<String>,
    client_owner: Option<omnifs_core::ClientOwnerId>,
    spec: Option<fs::Spec>,
    expected_record: Option<HelperRecord>,
    replaced: bool,
}

impl<'a> LibkrunLaunchLease<'a> {
    fn prepare_runtime_dir(&self) -> Result<()> {
        let dir = self.runner.dir();
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restrict {} to 0700", dir.display()))?;
        for path in [
            self.daemon_attach_socket.as_path(),
            self.runner.attach_bridge_socket().as_path(),
            self.runner.ready_socket().as_path(),
            self.runner.ssh_socket().as_path(),
            self.runner.control_socket().as_path(),
        ] {
            check_uds_path_length(path)?;
        }
        Ok(())
    }

    fn new(runner: &'a LibkrunRunner, daemon_attach_socket: &Path, guest_image: PathBuf) -> Self {
        Self {
            runner,
            daemon_attach_socket: daemon_attach_socket.to_path_buf(),
            guest_image,
            child: None,
            ready_listener: None,
            instance_id: None,
            client_owner: None,
            spec: None,
            expected_record: None,
            replaced: false,
        }
    }

    fn for_confirmed_teardown(runner: &'a LibkrunRunner, expected_record: HelperRecord) -> Self {
        Self {
            runner,
            daemon_attach_socket: PathBuf::new(),
            guest_image: PathBuf::new(),
            child: None,
            ready_listener: None,
            instance_id: None,
            client_owner: None,
            spec: None,
            expected_record: Some(expected_record),
            replaced: true,
        }
    }

    async fn prepare(runner: &'a LibkrunRunner, request: &LaunchRequest<'_>) -> Result<Self> {
        let guest_image = resolve_guest_image(
            request.assets.guest_image.as_deref(),
            request.paths.guest_image_cache(),
            request.events.clone(),
        )
        .await?;
        let mut lease = Self::new(runner, request.endpoints.attach_unix()?, guest_image);
        lease.client_owner = Some(request.client_owner);
        lease.spec = Some(request.spec.clone());
        Ok(lease)
    }

    async fn run(
        mut self,
        events: &RuntimeEventSink,
        attached: impl Future<Output = Result<()>>,
    ) -> Result<()> {
        match self.run_to_publish(attached, events).await {
            Ok(()) => {
                if let Some(spec) = &self.spec {
                    events.emit(RuntimeEvent::MountReady {
                        runtime: fs::Runtime::Libkrun,
                        id: spec.id().clone(),
                        location: spec.location().to_path_buf(),
                        container: None,
                    });
                }
                Ok(())
            },
            Err(error) => {
                if let Some(spec) = &self.spec {
                    emit_stage(events, spec, RuntimeStage::Stop, RuntimeState::Stopping);
                }
                let cleanup = if self.replaced {
                    self.stop_and_remove().await
                } else {
                    self.ready_listener.take();
                    Ok(())
                };
                err_after_rollback(error, cleanup, "the failed libkrun launch")
            },
        }
    }

    async fn run_to_publish(
        &mut self,
        attached: impl Future<Output = Result<()>>,
        events: &RuntimeEventSink,
    ) -> Result<()> {
        let installation = Installation::current()?;
        installation.probe()?;
        self.replace_stale().await?;

        self.prepare_runtime_dir()?;
        let dir = self.runner.dir();

        let launch_spec = self
            .spec
            .as_ref()
            .context("libkrun launch has no filesystem identity")?;
        emit_stage(
            events,
            launch_spec,
            RuntimeStage::MaterializeImage,
            RuntimeState::Active,
        );
        self.materialize_root_disk()?;
        let ssh_pubkey = self.runner.ensure_ssh_keypair()?;
        let spec = self
            .spec
            .as_ref()
            .context("libkrun launch has no filesystem identity")?
            .clone();
        let client_owner = self
            .client_owner
            .context("libkrun launch has no client owner identity")?;
        self.runner
            .write_seed_iso(client_owner, &spec, &ssh_pubkey)?;
        self.ready_listener = Some(self.bind_ready_listener()?);
        let _ = std::fs::remove_file(self.runner.ssh_socket());
        let _ = std::fs::remove_file(self.runner.control_socket());

        let instance_id = crate::process::new_instance_id("libkrun helper")?;
        let helper_config = omnifs_libkrun::Config::omnifs(
            dir,
            &self.daemon_attach_socket,
            spec.clone(),
            &instance_id,
            &installation,
        )?;
        self.instance_id = Some(instance_id);
        let mut command = Command::new(installation.helper());
        helper_config.apply_to(&mut command);
        crate::process::configure_detached_child(
            &mut command,
            helper_config.diagnostic_log(),
            crate::process::LogMode::TruncateRestricted0600,
        )?;

        // Detached: the VM outlives this CLI invocation. The lease retains
        // the pid until readiness publishes, while explicit teardown later
        // rediscovers it from the durable helper record.
        self.child = Some(command.spawn().with_context(|| {
            format!(
                "spawn packaged libkrun helper {}",
                installation.helper().display()
            )
        })?);

        let record = self.wait_for_helper_record().await?;
        let child_pid = self
            .child
            .as_ref()
            .context("libkrun child identity was lost before helper-record publication")?
            .id();
        anyhow::ensure!(
            child_pid == record.pid,
            "libkrun helper record named pid {}, but the spawned process is {child_pid}",
            record.pid
        );
        anyhow::ensure!(
            record.spec == spec,
            "libkrun helper record did not match the resolved filesystem spec"
        );
        anyhow::ensure!(
            self.instance_id.as_deref() == Some(record.instance_id.as_str()),
            "libkrun helper record did not match the launch instance"
        );
        self.runner.confirm_record(&record)?;

        self.wait_for_ready(events).await?;
        let spec = self
            .spec
            .as_ref()
            .context("libkrun launch has no filesystem identity")?;
        emit_stage(
            events,
            spec,
            RuntimeStage::WaitForVfsSession,
            RuntimeState::Active,
        );
        attached.await?;
        emit_stage(
            events,
            spec,
            RuntimeStage::WaitForVfsSession,
            RuntimeState::Ready,
        );
        Ok(())
    }

    fn materialize_root_disk(&self) -> Result<()> {
        let root = self.runner.root_raw();
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let part = self.runner.dir().join(format!(
            "{ROOT_RAW_PART_PREFIX}{}-{nonce}",
            std::process::id()
        ));

        let result = (|| {
            std::fs::copy(&self.guest_image, &part).with_context(|| {
                format!(
                    "copy immutable guest image {} to writable libkrun root {}",
                    self.guest_image.display(),
                    part.display()
                )
            })?;
            std::fs::set_permissions(&part, std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("restrict {} to 0600", part.display()))?;
            std::fs::rename(&part, &root).with_context(|| {
                format!(
                    "publish writable libkrun root {} from {}",
                    root.display(),
                    part.display()
                )
            })?;
            Ok::<(), anyhow::Error>(())
        })();

        if result.is_err() {
            let _ = std::fs::remove_file(&part);
        }
        result
    }

    async fn replace_stale(&mut self) -> Result<()> {
        self.stop_and_remove()
            .await
            .context("tear down a prior libkrun instance before relaunch")?;
        self.replaced = true;
        Ok(())
    }

    fn bind_ready_listener(&self) -> Result<tokio::net::UnixListener> {
        let path = self.runner.ready_socket();
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)
            .with_context(|| format!("bind readiness listener {}", path.display()))?;
        listener
            .set_nonblocking(true)
            .context("configure the readiness listener")?;
        tokio::net::UnixListener::from_std(listener)
            .context("adopt the readiness listener into the async runtime")
    }

    async fn wait_for_helper_record(&mut self) -> Result<HelperRecord> {
        let ready = crate::process::poll_until_mut(
            HELPER_RECORD_TIMEOUT,
            HELPER_POLL_INTERVAL,
            self,
            |lease| {
                Box::pin(async move {
                    if let Ok(Some(record)) = lease.runner.read_helper_record()
                        && lease.instance_id.as_deref() == Some(record.instance_id.as_str())
                    {
                        return Ok(Some(record));
                    }
                    if let Some(status) = lease
                        .child
                        .as_mut()
                        .context(
                            "libkrun helper identity was lost before helper-record publication",
                        )?
                        .try_wait()
                        .context("poll libkrun helper before helper-record publication")?
                    {
                        anyhow::bail!(
                            "omnifs-libkrun exited with {status} before publishing {};\n{}",
                            lease.runner.pidfile().display(),
                            lease.runner.diagnostic_tail()
                        );
                    }
                    Ok(None)
                })
            },
        )
        .await?;
        ready.with_context(|| {
            format!(
                "omnifs-libkrun did not publish {} within {}s;\n{}",
                self.runner.pidfile().display(),
                HELPER_RECORD_TIMEOUT.as_secs(),
                self.runner.diagnostic_tail()
            )
        })
    }

    async fn wait_for_ready(&mut self, events: &RuntimeEventSink) -> Result<()> {
        let spec = self
            .spec
            .as_ref()
            .context("libkrun launch has no filesystem identity")?;
        emit_stage(
            events,
            spec,
            RuntimeStage::WaitForOsMount,
            RuntimeState::Active,
        );
        let listener = self
            .ready_listener
            .take()
            .context("libkrun readiness listener was not prepared")?;
        let wait = async {
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (mut stream, _) = accepted?;
                        let mut buf = [0_u8; 64];
                        let n = stream.read(&mut buf).await?;
                        if buf[..n].starts_with(b"ready") {
                            return Ok::<(), anyhow::Error>(());
                        }
                    }
                    () = tokio::time::sleep(HELPER_POLL_INTERVAL) => {
                        if let Some(status) = self
                            .child
                            .as_mut()
                            .context("libkrun helper identity was lost while waiting for readiness")?
                            .try_wait()
                            .context("poll libkrun helper while waiting for readiness")?
                        {
                            anyhow::bail!(
                                "omnifs-libkrun exited with {status} before guest readiness;\n{}",
                                self.runner.diagnostic_tail()
                            );
                        }
                    }
                }
            }
        };
        if let Ok(result) = tokio::time::timeout(LAUNCH_READY_TIMEOUT, wait).await {
            result.context("read the libkrun readiness beacon")?;
            emit_stage(
                events,
                spec,
                RuntimeStage::WaitForOsMount,
                RuntimeState::Ready,
            );
            Ok(())
        } else {
            anyhow::bail!(
                "{} did not appear inside the filesystem within {}s",
                fs::GUEST_LOCATION,
                LAUNCH_READY_TIMEOUT.as_secs()
            )
        }
    }

    async fn stop_and_remove(&mut self) -> Result<()> {
        self.ready_listener.take();
        let record = self.runner.read_helper_record()?;
        if let Some(expected) = self.expected_record.as_ref() {
            ensure_identity_unchanged(record.as_ref(), expected, "libkrun helper")?;
        }
        if let Some(expected) = record.as_ref() {
            self.runner.confirm_record(expected)?;
            ControlSocket::new(self.runner.control_socket())?
                .request_shutdown(expected)
                .context("request identity-matched libkrun shutdown")?;
        } else if self.child.is_none() {
            // With no helper identity there is no safe detached action. Do
            // not unlink sockets or disks that a racing helper may own.
            return Ok(());
        }

        if self.child.is_some() {
            if !self.wait_for_owned_child_exit(CHILD_EXIT_TIMEOUT).await? {
                let child = self
                    .child
                    .as_mut()
                    .context("libkrun child identity was lost during launch rollback")?;
                let pid = child.id();
                child
                    .kill()
                    .with_context(|| format!("kill directly-owned libkrun child {pid}"))?;
                anyhow::ensure!(
                    self.wait_for_owned_child_exit(CHILD_KILL_TIMEOUT).await?,
                    "directly-owned libkrun child {pid} remained live after termination; \
                     recovery identity was preserved"
                );
            }
        } else if let Some(expected) = record.as_ref() {
            self.wait_for_detached_exit(expected, DETACHED_EXIT_TIMEOUT)
                .await?;
        }

        self.child = None;
        anyhow::ensure!(
            self.runner.read_helper_record()?.is_none(),
            "libkrun helper exited without removing its identity record; recovery artifacts were preserved"
        );
        self.remove_owned_artifacts();
        Ok(())
    }

    async fn wait_for_owned_child_exit(&mut self, timeout: Duration) -> Result<bool> {
        Ok(
            crate::process::poll_until_mut(timeout, HELPER_POLL_INTERVAL, self, |lease| {
                Box::pin(async move {
                    let exited = lease
                        .child
                        .as_mut()
                        .context("libkrun child identity was lost while waiting for exit")?
                        .try_wait()?
                        .is_some();
                    Ok(exited.then_some(()))
                })
            })
            .await?
            .is_some(),
        )
    }

    async fn wait_for_detached_exit(
        &self,
        expected: &HelperRecord,
        timeout: Duration,
    ) -> Result<()> {
        crate::process::poll_until(timeout, HELPER_POLL_INTERVAL, || async {
            match self.runner.read_helper_record()? {
                None => Ok(Some(())),
                Some(current) if current != *expected => {
                    anyhow::bail!(
                        "libkrun helper identity changed while waiting for shutdown; recovery artifacts were preserved"
                    );
                },
                Some(_) => Ok(None),
            }
        })
        .await?
        .with_context(|| {
            format!(
                "identity-matched libkrun helper did not finish shutdown within {}s; recovery artifacts were preserved",
                timeout.as_secs()
            )
        })
    }

    /// Remove only launch artifacts. The daemon attach listener, verified
    /// guest image, and persistent SSH key are owned elsewhere and never
    /// appear in this set.
    fn remove_owned_artifacts(&self) {
        for path in [
            self.runner.root_raw(),
            self.runner.seed_iso(),
            self.runner.ssh_socket(),
            self.runner.ready_socket(),
            self.runner.control_socket(),
            self.runner.attach_bridge_socket(),
            self.runner.serial_log(),
            self.runner.diagnostic_log(),
        ] {
            let _ = std::fs::remove_file(path);
        }
        for path in self.runner.root_raw_parts() {
            let _ = std::fs::remove_file(path);
        }
        let _ = std::fs::remove_dir_all(self.runner.seed_staging());
    }
}

impl LibkrunRunner {
    pub(crate) async fn launch(
        &self,
        request: &LaunchRequest<'_>,
        attached: impl Future<Output = Result<()>>,
    ) -> Result<()> {
        let lease = LibkrunLaunchLease::prepare(self, request).await?;
        emit_stage(
            request.events,
            request.spec,
            RuntimeStage::StartVm,
            RuntimeState::Active,
        );
        lease.run(request.events, attached).await
    }
}

impl LibkrunRunner {
    fn confirm_record(&self, expected: &HelperRecord) -> Result<()> {
        let actual = ControlSocket::new(self.control_socket())?
            .ping(&expected.spec, &expected.instance_id)
            .context("prove libkrun helper identity")?;
        anyhow::ensure!(
            actual == *expected,
            "libkrun helper record and control identity do not match"
        );
        Ok(())
    }

    /// Prove a live, identity-matched helper, matching the host and Docker
    /// drivers' `confirmed`.
    pub fn confirmed(&self) -> Result<Option<HelperRecord>> {
        let Some(record) = self.read_helper_record()? else {
            anyhow::ensure!(
                !self.has_operational_state(),
                "libkrun filesystem state exists at {} without a helper record",
                self.dir().display()
            );
            return Ok(None);
        };
        self.confirm_record(&record)?;
        Ok(Some(record))
    }

    /// The one teardown entry point for a proven identity, matching the host
    /// and Docker drivers' `stop_confirmed`.
    pub async fn stop_confirmed(&self, expected: HelperRecord) -> Result<()> {
        LibkrunLaunchLease::for_confirmed_teardown(self, expected)
            .stop_and_remove()
            .await
    }

    /// Scan every filesystem id directory under `runtime_root` for one that
    /// still carries a `libkrun` state directory, and prove each one's
    /// helper record identity. Not a `&self` method: it enumerates every
    /// filesystem's libkrun runtime, not just one instance's, matching the
    /// host driver's `owned` and Docker's `owned`.
    pub fn owned(runtime_root: &Path) -> Vec<Candidate> {
        let mut owned = Vec::new();
        let entries = match std::fs::read_dir(runtime_root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return owned,
            Err(error) => {
                owned.push(Candidate::Invalid {
                    backend: "libkrun",
                    target: Some(runtime_root.display().to_string()),
                    error: error.to_string(),
                });
                return owned;
            },
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    owned.push(Candidate::Invalid {
                        backend: "libkrun",
                        target: None,
                        error: error.to_string(),
                    });
                    continue;
                },
            };
            let state_dir = entry.path().join("libkrun");
            if !state_dir.exists() {
                continue;
            }
            let raw_id = entry.file_name().to_string_lossy().into_owned();
            let id = match fs::Id::new(raw_id.clone()) {
                Ok(id) => id,
                Err(error) => {
                    owned.push(Candidate::Invalid {
                        backend: "libkrun",
                        target: Some(raw_id),
                        error: error.to_string(),
                    });
                    continue;
                },
            };
            let confirmed = Self::new(state_dir.clone())
                .confirmed()
                .map_err(|error| format!("{error:#}"));
            owned.push(Candidate::Libkrun {
                id,
                state_dir,
                confirmed,
            });
        }
        owned
    }

    /// Pure command construction: no I/O. Callers that are about to actually
    /// run this must probe for `socat` themselves (`ensure_socat_available`).
    pub fn shell_command(&self, shell_override: Option<&str>, trailing: &[String]) -> Command {
        let mut cmd = Command::new("ssh");
        cmd.arg("-i")
            .arg(self.ssh_key_path())
            .arg("-o")
            .arg("IdentitiesOnly=yes")
            .arg("-o")
            .arg("StrictHostKeyChecking=no")
            .arg("-o")
            .arg("UserKnownHostsFile=/dev/null")
            .arg("-o")
            .arg("LogLevel=ERROR")
            .arg("-o")
            .arg(format!(
                "ProxyCommand=socat - UNIX-CONNECT:{}",
                self.ssh_socket().display()
            ));
        if trailing.is_empty() {
            cmd.arg("-t");
        }
        cmd.arg(SSH_GUEST_TARGET);
        let program = if trailing.is_empty() {
            vec![shell_override.unwrap_or("/bin/sh").to_owned()]
        } else {
            trailing.to_vec()
        };
        let remote = format!(
            "cd {} && exec {}",
            shell_word(fs::GUEST_LOCATION),
            program
                .iter()
                .map(|word| shell_word(word))
                .collect::<Vec<_>>()
                .join(" ")
        );
        cmd.arg(remote);
        cmd
    }
}

/// Quote one argv element for the POSIX shell used by OpenSSH's remote
/// command. OpenSSH exposes one remote command string, so quoting each word is
/// required to preserve the CLI's argv boundary.
fn shell_word(word: &str) -> String {
    if !word.is_empty()
        && word
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&byte))
    {
        return word.to_owned();
    }
    format!("'{}'", word.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn guest_image_resolution_precedence() {
        // Tests run under a dev build (no OMNIFS_RELEASE at compile time), so
        // the configured path is resolved locally. The release path is
        // covered by `default_guest_image_for` directly, mirroring the
        // filesystem image tests.
        let temp = tempfile::tempdir().unwrap();
        let custom = temp.path().join("custom.raw");
        std::fs::write(&custom, b"guest image").unwrap();
        let configured = custom.to_string_lossy().into_owned();
        let image =
            resolve_guest_image(Some(&configured), temp.path(), RuntimeEventSink::discard())
                .await
                .unwrap();
        assert_eq!(image, custom);
    }

    #[tokio::test]
    async fn post_beacon_attachment_failure_rolls_back_invocation_resources() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("home").join("libkrun");
        std::fs::create_dir_all(&dir).unwrap();
        let attach_socket = temp.path().join("daemon-attach.sock");
        std::fs::write(&attach_socket, b"daemon-owned").unwrap();
        std::fs::write(dir.join(SSH_KEY_NAME), b"persistent key").unwrap();
        let guest_image = dir.join("base.raw");
        std::fs::write(&guest_image, b"immutable guest image").unwrap();
        std::fs::set_permissions(&guest_image, std::fs::Permissions::from_mode(0o444)).unwrap();

        let runner = LibkrunRunner::new(dir.clone());
        let lease = LibkrunLaunchLease::new(&runner, &attach_socket, guest_image.clone());
        lease.materialize_root_disk().unwrap();
        assert_eq!(
            std::fs::metadata(runner.root_raw())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let root_part = dir.join(format!("{ROOT_RAW_PART_PREFIX}fixture"));
        std::fs::write(&root_part, b"partial root").unwrap();
        for name in [
            SEED_DISK_NAME,
            SSH_SOCKET_NAME,
            READY_SOCKET_NAME,
            CONTROL_SOCKET_NAME,
            ATTACH_BRIDGE_SOCKET_NAME,
            SERIAL_LOG_NAME,
            DIAGNOSTIC_LOG_NAME,
        ] {
            std::fs::write(dir.join(name), b"launch-owned").unwrap();
        }
        std::fs::create_dir_all(dir.join(SEED_STAGING_NAME)).unwrap();

        let mut lease = lease;
        lease.replaced = true;
        lease.child = Some(
            std::process::Command::new("sleep")
                .arg("1")
                .spawn()
                .unwrap(),
        );
        let pid = lease.child.as_ref().unwrap().id();
        let attachment = async {
            Err::<(), _>(anyhow::anyhow!(
                "daemon attachment failed after the readiness beacon"
            ))
        };
        let error = attachment.await.unwrap_err();
        assert!(error.to_string().contains("after the readiness beacon"));
        lease.stop_and_remove().await.unwrap();

        assert!(!process_alive(pid));
        assert!(attach_socket.is_file());
        assert!(dir.join(SSH_KEY_NAME).is_file());
        assert!(guest_image.is_file());
        assert_eq!(
            std::fs::metadata(&guest_image)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o444
        );
        assert!(!dir.join(PID_FILE_NAME).exists());
        assert!(!dir.join(ROOT_DISK_NAME).exists());
        assert!(!root_part.exists());
        assert!(!dir.join(SEED_DISK_NAME).exists());
        assert!(!dir.join(SEED_STAGING_NAME).exists());
    }

    #[test]
    fn dev_channel_defaults_to_local_guest_image_path() {
        assert_eq!(
            default_guest_image_for(BuildChannel::Dev),
            DEFAULT_GUEST_IMAGE
        );
    }

    #[test]
    fn release_channel_defaults_to_pinned_guest_image_registry_tag() {
        assert!(
            default_guest_image_for(BuildChannel::Release)
                .starts_with("ghcr.io/0xff-ai/omnifs-guest:")
        );
    }

    #[test]
    fn remote_shell_command_preserves_each_argv_element() {
        let runner = LibkrunRunner::new(PathBuf::from("/tmp/omnifs-libkrun-test"));
        let command = runner.shell_command(
            None,
            &[
                "printf".to_owned(),
                "two words".to_owned(),
                "single'quote".to_owned(),
            ],
        );
        let remote = command
            .get_args()
            .last()
            .expect("remote command")
            .to_string_lossy();
        assert_eq!(
            remote,
            "cd /omnifs && exec printf 'two words' 'single'\"'\"'quote'"
        );
    }

    #[test]
    fn seed_audit_accepts_the_exact_expected_key_set() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(SEED_CONF_NAME),
            "OMNIFS_ATTACH_ADDR=vsock:1024\n\
             OMNIFS_CLIENT_OWNER=0123456789abcdef0123456789abcdef\n\
             OMNIFS_FS_ID=main\n\
             OMNIFS_READY_VSOCK_PORT=1025\n\
             OMNIFS_SSH_PUBKEY=ssh-ed25519 AAAA test\n",
        )
        .unwrap();
        audit_seed_staging(dir.path()).expect("the exact expected key set must pass");
    }

    #[test]
    fn seed_audit_rejects_an_extra_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(SEED_CONF_NAME),
            "OMNIFS_ATTACH_ADDR=vsock:1024\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("extra.txt"), "surprise").unwrap();
        let err = audit_seed_staging(dir.path()).unwrap_err();
        assert!(err.contains("exactly one file"));
    }

    #[test]
    fn seed_audit_rejects_an_unexpected_key() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(SEED_CONF_NAME),
            "OMNIFS_ATTACH_ADDR=vsock:1024\n\
             OMNIFS_READY_VSOCK_PORT=1025\n\
             OMNIFS_SSH_PUBKEY=ssh-ed25519 AAAA test\n\
             OMNIFS_HOME=/root/.omnifs\n",
        )
        .unwrap();
        let err = audit_seed_staging(dir.path()).unwrap_err();
        assert!(err.contains("OMNIFS_HOME"));
    }

    #[test]
    fn seed_audit_rejects_a_missing_key() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(SEED_CONF_NAME),
            "OMNIFS_ATTACH_ADDR=vsock:1024\n",
        )
        .unwrap();
        let err = audit_seed_staging(dir.path()).unwrap_err();
        assert!(err.contains("missing required key"));
    }
}
