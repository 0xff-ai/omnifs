//! The libkrun frontend runner: a macOS microVM hosting the same
//! `omnifs-thin fuse` runner and Omnifs VFS wire protocol the Docker runner runs in a
//! container, attached to the host-native daemon's namespace over vsock
//! instead of TCP.
//!
//! State lives under `<config_dir>/libkrun/`: a persistent per-workspace ed25519
//! keypair (survives across launches, since it authenticates guest ssh access
//! independent of any one VM instance) plus per-launch artifacts (a writable
//! root disk, pidfile, seed ISO, the helper-owned attach bridge, the readiness,
//! SSH, and control sockets, and the serial log). Every path lives under the workspace config dir,
//! never a system temp dir, so `omnifs down`/`frontend disable` can find and
//! remove exactly what this runner owns. The resolved guest image is an
//! immutable base artifact and is only the source for that launch-local root.
//!
//! One explicit no-TSI vsock device bridges the guest to the host, with three
//! fixed port mappings:
//! - port 1024 (attach): guest-initiated (`,listen`) onto the daemon's own
//!   vsock-attach unix socket (returned by the `AttachVsock` control operation;
//!   this runner never creates or removes that socket).
//! - port 1025 (ready): guest-initiated (`,listen`) onto a unix socket this
//!   runner binds before spawning libkrun; the launch lease accepts one later
//!   readiness beacon on it — a
//!   `,listen` device requires the host side already listening, since libkrun
//!   dials it once per guest connection rather than the reverse.
//! - port 22 (ssh): host-initiated (`,connect`, libkrun's explicit
//!   host-to-guest mode; omitting both keywords means guest-initiated):
//!   libkrun itself creates and listens on the unix socket, relaying each
//!   accepted connection into the guest's vsock-listening dropbear
//!   (`ListenStream=vsock::22` in the guest image). `omnifs frontend shell` dials it
//!   through `ssh -o ProxyCommand='socat - UNIX-CONNECT:<path>'`.
//!
//! No network or GPU configuration exists in the helper's typed launch shape.
//! The helper disables libkrun's implicit TSI vsock before adding the explicit
//! device, so the guest gets neither ordinary network egress nor TSI socket
//! hijacking.

use std::future::Future;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use omnifs_libkrun::{
    ATTACH_BRIDGE_SOCKET_NAME, CONTROL_SOCKET_NAME, ControlSocket, DIAGNOSTIC_LOG_NAME,
    Installation, PID_FILE_NAME, READY_SOCKET_NAME, ROOT_DISK_NAME, SEED_DISK_NAME,
    SERIAL_LOG_NAME, SSH_SOCKET_NAME,
};
use tokio::io::AsyncReadExt as _;

use crate::commands::frontend::GUEST_MOUNT;
use crate::image::{BUILD_CHANNEL, BuildChannel, ImageRef};
use crate::process::is_alive as process_alive;
use crate::ui::output::Output;
use omnifs_workspace::config::Config;

const SSH_KEY_NAME: &str = "id_ed25519";
const ROOT_RAW_PART_PREFIX: &str = "root.raw.part.";
const SEED_STAGING_NAME: &str = "seed-staging";

/// Guest vsock port the daemon's attach listener is proxied onto.
const ATTACH_VSOCK_PORT: u32 = 1024;
/// Guest vsock port used by the readiness beacon in `omnifs-vfs-wire`.
/// dials once the FUSE mount is serving.
const READY_VSOCK_PORT: u32 = 1025;

/// A placeholder hostname for the ssh command line. `ProxyCommand` replaces
/// the transport entirely, so this name is never resolved or dialed.
const SSH_GUEST_TARGET: &str = "root@omnifs-guest";

const ENV_GUEST_IMAGE: &str = "OMNIFS_GUEST_IMAGE";
/// The `just guest-image` recipe's default output path
/// (`scripts/guest-image/build.sh`'s `OUT_DIR` default), resolved relative to
/// the current working directory. A repo-root-relative default matches every
/// other dev-only default in this crate (e.g. `omnifs-frontend:dev`) rather
/// than trying to locate the repo from an installed binary.
const DEFAULT_GUEST_IMAGE: &str = "target/guest-image/omnifs-guest.raw";
/// Release channel default: the pinned ghcr OCI artifact tag the
/// guest-image-arm64 CI job publishes and `release`'s `promote` job retags
/// to this version (mirrors `FRONTEND_RELEASE_IMAGE`'s version pinning).
const GUEST_RELEASE_IMAGE: &str =
    concat!("ghcr.io/0xff-ai/omnifs-guest:", env!("CARGO_PKG_VERSION"));

const SEED_VOLUME_LABEL: &str = "OMNIFS-SEED";
const SEED_CONF_NAME: &str = "omnifs-seed.conf";

/// The exact seed keys a launch ever writes. The lockdown audit
/// ([`audit_seed_staging`]) asserts the staging dir carries exactly this set
/// before it is burned into the ISO: only `OMNIFS_ATTACH_TOKEN` is sensitive,
/// so bounding the key set is what stands between "seed" and "accidental
/// credential leak into a guest-readable volume."
const SEED_CONF_KEYS: [&str; 4] = [
    "OMNIFS_ATTACH_ADDR",
    "OMNIFS_ATTACH_TOKEN",
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
/// `default_frontend_image_for`.
pub(crate) const fn default_guest_image_for(channel: BuildChannel) -> &'static str {
    match channel {
        BuildChannel::Release => GUEST_RELEASE_IMAGE,
        BuildChannel::Dev => DEFAULT_GUEST_IMAGE,
    }
}

/// `omnifs frontend shell`'s libkrun dispatch calls this before building the ssh
/// command: `shell_command` itself stays pure construction (no I/O), so the
/// probe belongs at the one call site that is about to actually run it.
pub(crate) fn ensure_socat_available() -> Result<()> {
    match Command::new("socat")
        .arg("-V")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => anyhow::bail!(
            "socat is required to reach the libkrun guest over vsock; install it with `brew install socat`"
        ),
        Err(error) => Err(error).context("probe for socat on PATH"),
    }
}

/// The libkrun microVM frontend runner. Durable workspace state and explicit
/// teardown live here; one launch's resources live in [`LibkrunLaunchLease`].
pub(crate) struct LibkrunRunner {
    dir: PathBuf,
}

impl LibkrunRunner {
    pub(crate) fn probe() -> Result<()> {
        Installation::current()?.probe()?;
        Ok(())
    }

    pub(crate) fn new(dir: PathBuf) -> Self {
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

    /// Generate the per-workspace ed25519 keypair if absent, returning the
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
    fn write_seed_iso(&self, attach_token: &str, ssh_pubkey: &str) -> Result<()> {
        let staging = self.seed_staging();
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging)
            .with_context(|| format!("create seed staging dir {}", staging.display()))?;

        let conf_path = staging.join(SEED_CONF_NAME);
        let conf = format!(
            "OMNIFS_ATTACH_ADDR=vsock:{ATTACH_VSOCK_PORT}\n\
             OMNIFS_ATTACH_TOKEN={attach_token}\n\
             OMNIFS_READY_VSOCK_PORT={READY_VSOCK_PORT}\n\
             OMNIFS_SSH_PUBKEY={ssh_pubkey}\n"
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

    fn read_pidfile(&self) -> Result<Option<u32>> {
        match std::fs::read_to_string(self.pidfile()) {
            Ok(contents) => Ok(Some(
                contents
                    .trim()
                    .parse::<u32>()
                    .context("parse the libkrun pidfile")?,
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).context("read libkrun pidfile"),
        }
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
    config: &Config,
    guest_image_cache: &Path,
    output: Output,
) -> Result<PathBuf> {
    let resolved = std::env::var(ENV_GUEST_IMAGE)
        .ok()
        .or_else(|| config.frontend.guest_image.clone())
        .unwrap_or_else(|| default_guest_image_for(BUILD_CHANNEL).to_string());
    let path = match BUILD_CHANNEL {
        BuildChannel::Dev => PathBuf::from(resolved),
        BuildChannel::Release => {
            crate::guest_image_pull::ensure_guest_image(
                &ImageRef::new(resolved)?,
                guest_image_cache,
                output,
            )
            .await?
        },
    };
    anyhow::ensure!(
        path.is_file(),
        "guest image not found at {}; build it with `just guest-image` \
         (see docs/contracts/60-build-validation.md)",
        path.display()
    );
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
    attach_token: String,
    mount: Option<String>,
    timeout: Duration,
    child: Option<std::process::Child>,
    ready_listener: Option<tokio::net::UnixListener>,
    replaced: bool,
}

pub(crate) struct LibkrunLaunchRequest<'a> {
    pub(crate) daemon_attach_socket: &'a Path,
    pub(crate) attach_token: &'a str,
    pub(crate) config: &'a Config,
    pub(crate) guest_image_cache: &'a Path,
    pub(crate) output: Output,
    pub(crate) mount: Option<&'a str>,
    pub(crate) timeout: Duration,
}

impl<'a> LibkrunLaunchLease<'a> {
    fn new(runner: &'a LibkrunRunner, daemon_attach_socket: &Path, guest_image: PathBuf) -> Self {
        Self {
            runner,
            daemon_attach_socket: daemon_attach_socket.to_path_buf(),
            guest_image,
            attach_token: String::new(),
            mount: None,
            timeout: Duration::ZERO,
            child: None,
            ready_listener: None,
            replaced: false,
        }
    }

    fn for_teardown(runner: &'a LibkrunRunner) -> Self {
        Self {
            runner,
            daemon_attach_socket: PathBuf::new(),
            guest_image: PathBuf::new(),
            attach_token: String::new(),
            mount: None,
            timeout: Duration::ZERO,
            child: None,
            ready_listener: None,
            replaced: true,
        }
    }

    async fn prepare(runner: &'a LibkrunRunner, request: LibkrunLaunchRequest<'_>) -> Result<Self> {
        let guest_image =
            resolve_guest_image(request.config, request.guest_image_cache, request.output).await?;
        let mut lease = Self::new(runner, request.daemon_attach_socket, guest_image);
        request.attach_token.clone_into(&mut lease.attach_token);
        lease.mount = request.mount.map(str::to_owned);
        lease.timeout = request.timeout;
        Ok(lease)
    }

    async fn run(mut self, attached: impl Future<Output = Result<()>>) -> Result<()> {
        let result = self.run_to_publish(attached).await;
        match result {
            Ok(()) => Ok(()),
            Err(error) => {
                let cleanup = if self.replaced {
                    self.stop_and_remove().await
                } else {
                    self.ready_listener.take();
                    Ok(())
                };
                match cleanup {
                    Ok(()) => Err(error),
                    Err(cleanup) => {
                        Err(error
                            .context(format!("libkrun launch rollback also failed: {cleanup:#}")))
                    },
                }
            },
        }
    }

    async fn run_to_publish(&mut self, attached: impl Future<Output = Result<()>>) -> Result<()> {
        let installation = Installation::current()?;
        installation.probe()?;
        self.replace_stale().await?;

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

        self.materialize_root_disk()?;
        let ssh_pubkey = self.runner.ensure_ssh_keypair()?;
        self.runner
            .write_seed_iso(&self.attach_token, &ssh_pubkey)?;
        self.ready_listener = Some(self.bind_ready_listener()?);
        let _ = std::fs::remove_file(self.runner.ssh_socket());
        let _ = std::fs::remove_file(self.runner.control_socket());

        let helper_config =
            omnifs_libkrun::Config::omnifs(dir, &self.daemon_attach_socket, &installation)?;
        let diagnostic_path = helper_config.diagnostic_log();
        let diagnostic = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(diagnostic_path)
            .with_context(|| format!("open helper log {}", diagnostic_path.display()))?;
        let diagnostic_stderr = diagnostic
            .try_clone()
            .with_context(|| format!("clone helper log {}", diagnostic_path.display()))?;
        let mut command = Command::new(installation.helper());
        helper_config.apply_to(&mut command);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::from(diagnostic))
            .stderr(Stdio::from(diagnostic_stderr));
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command.process_group(0);
        }

        // Detached: the VM outlives this CLI invocation. The lease retains
        // the pid until readiness publishes, while explicit teardown later
        // rediscovers it from the durable pidfile.
        self.child = Some(command.spawn().with_context(|| {
            format!(
                "spawn packaged libkrun helper {}",
                installation.helper().display()
            )
        })?);

        let pid = self.wait_for_pidfile().await?;
        let child_pid = self
            .child
            .as_ref()
            .context("libkrun child identity was lost before pidfile publication")?
            .id();
        anyhow::ensure!(
            child_pid == pid,
            "libkrun pidfile named pid {pid}, but the spawned process is {child_pid}"
        );

        let mount = self.mount.clone();
        self.wait_for_ready(mount.as_deref(), self.timeout).await?;
        attached.await
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

    async fn wait_for_pidfile(&mut self) -> Result<u32> {
        let pidfile = self.runner.pidfile();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(contents) = std::fs::read_to_string(&pidfile)
                && let Ok(pid) = contents.trim().parse::<u32>()
            {
                return Ok(pid);
            }
            if let Some(status) = self
                .child
                .as_mut()
                .context("libkrun helper identity was lost before pidfile publication")?
                .try_wait()
                .context("poll libkrun helper before pidfile publication")?
            {
                anyhow::bail!(
                    "omnifs-libkrun exited with {status} before publishing {};\n{}",
                    pidfile.display(),
                    self.runner.diagnostic_tail()
                );
            }
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "omnifs-libkrun did not publish {} within 5s;\n{}",
                pidfile.display(),
                self.runner.diagnostic_tail()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn wait_for_ready(&mut self, mount: Option<&str>, timeout: Duration) -> Result<()> {
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
                    () = tokio::time::sleep(Duration::from_millis(100)) => {
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
        if let Ok(result) = tokio::time::timeout(timeout, wait).await {
            result.context("read the libkrun readiness beacon")
        } else {
            let path = mount.map_or_else(
                || GUEST_MOUNT.to_owned(),
                |name| format!("{GUEST_MOUNT}/{name}"),
            );
            anyhow::bail!(
                "{path} did not appear inside the frontend within {}s",
                timeout.as_secs()
            )
        }
    }

    async fn stop_and_remove(&mut self) -> Result<()> {
        self.ready_listener.take();
        let pid = match self.child.as_ref() {
            Some(child) => Some(child.id()),
            None => match self.runner.read_pidfile() {
                Ok(pid) => pid,
                Err(error) => return Err(error),
            },
        };
        if let Some(pid) = pid.filter(|pid| process_alive(*pid)) {
            let _ = ControlSocket::new(self.runner.control_socket())
                .and_then(|control| control.request_shutdown());
            if !self
                .wait_for_process_exit(pid, Duration::from_secs(5))
                .await?
            {
                let _ = Command::new("kill")
                    .arg("-TERM")
                    .arg(pid.to_string())
                    .status();
            }
            if !self
                .wait_for_process_exit(pid, Duration::from_secs(5))
                .await?
            {
                if let Some(child) = self.child.as_mut() {
                    child
                        .kill()
                        .with_context(|| format!("kill libkrun process {pid}"))?;
                } else {
                    let status = Command::new("kill")
                        .arg("-KILL")
                        .arg(pid.to_string())
                        .status()
                        .with_context(|| format!("kill libkrun process {pid}"))?;
                    anyhow::ensure!(
                        status.success(),
                        "kill -KILL failed for live libkrun process {pid}"
                    );
                }
                if !self
                    .wait_for_process_exit(pid, Duration::from_secs(3))
                    .await?
                {
                    anyhow::bail!(
                        "libkrun process {pid} remained live after termination; \
                         recovery identity was preserved"
                    );
                }
            }
        }
        if let Some(child) = self.child.as_mut()
            && child.try_wait()?.is_none()
        {
            let pid = child.id();
            anyhow::bail!(
                "libkrun process {pid} remained live after termination; recovery identity was preserved"
            );
        }
        self.child = None;
        self.remove_owned_artifacts();
        Ok(())
    }

    async fn wait_for_process_exit(&mut self, pid: u32, timeout: Duration) -> Result<bool> {
        let until = tokio::time::Instant::now() + timeout;
        loop {
            let exited = match self.child.as_mut() {
                Some(child) => child.try_wait()?.is_some(),
                None => !process_alive(pid),
            };
            if exited {
                return Ok(true);
            }
            if tokio::time::Instant::now() >= until {
                return Ok(false);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Remove only launch artifacts. The daemon attach listener, verified
    /// guest image, and persistent SSH key are owned elsewhere and never
    /// appear in this set.
    fn remove_owned_artifacts(&self) {
        for path in [
            self.runner.pidfile(),
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
        request: LibkrunLaunchRequest<'_>,
        attached: impl Future<Output = Result<()>>,
    ) -> Result<()> {
        LibkrunLaunchLease::prepare(self, request)
            .await?
            .run(attached)
            .await
    }
}

impl LibkrunRunner {
    pub(crate) fn is_running(&self) -> Result<Option<bool>> {
        let Some(pid) = self.read_pidfile()? else {
            return Ok(None);
        };
        Ok(Some(process_alive(pid)))
    }

    pub(crate) async fn tear_down(&self) -> Result<()> {
        LibkrunLaunchLease::for_teardown(self)
            .stop_and_remove()
            .await
    }

    /// Pure command construction: no I/O. Callers that are about to actually
    /// run this must probe for `socat` themselves (`ensure_socat_available`).
    pub(crate) fn shell_command(
        &self,
        shell_override: Option<&str>,
        trailing: &[String],
    ) -> Command {
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
        // ssh has no argv-array remote exec (unlike `docker exec`): every
        // trailing argument is space-joined into one remote command string,
        // so an argument containing embedded spaces can still split apart on
        // the guest. Acceptable for the same reason most ssh-wrapping CLIs
        // accept it: building real remote shell-quoting here would trade one
        // narrow edge case for a much larger footgun surface.
        cmd.arg("cd").arg(GUEST_MOUNT).arg("&&").arg("exec");
        if trailing.is_empty() {
            cmd.arg(shell_override.unwrap_or("/bin/sh"));
        } else {
            cmd.args(trailing);
        }
        cmd
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn guest_image_resolution_precedence() {
        // Tests run under a dev build (no OMNIFS_RELEASE at compile time), so
        // the configured path is resolved locally. The release path is
        // covered by `default_guest_image_for` directly, mirroring the
        // frontend image tests.
        let temp = tempfile::tempdir().unwrap();
        let custom = temp.path().join("custom.raw");
        std::fs::write(&custom, b"guest image").unwrap();
        let mut config = Config::default();
        config.frontend.guest_image = Some(custom.to_string_lossy().into_owned());
        let image = resolve_guest_image(
            &config,
            temp.path(),
            Output::new(crate::ui::output::OutputMode::Human, false),
        )
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
            PID_FILE_NAME,
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
    fn seed_audit_accepts_the_exact_expected_key_set() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(SEED_CONF_NAME),
            "OMNIFS_ATTACH_ADDR=vsock:1024\n\
             OMNIFS_ATTACH_TOKEN=abc123\n\
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
             OMNIFS_ATTACH_TOKEN=abc123\n\
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
            "OMNIFS_ATTACH_ADDR=vsock:1024\nOMNIFS_ATTACH_TOKEN=abc123\n",
        )
        .unwrap();
        let err = audit_seed_staging(dir.path()).unwrap_err();
        assert!(err.contains("missing required key"));
    }
}
