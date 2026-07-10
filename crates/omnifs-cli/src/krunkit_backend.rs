//! The krunkit (libkrun) frontend backend: a macOS microVM hosting the same
//! `omnifs-fuse` binary and wire protocol the Docker backend runs in a
//! container, attached to the host-native daemon's namespace over vsock
//! instead of TCP.
//!
//! State lives under `<config_dir>/krunkit/`: a persistent per-workspace ed25519
//! keypair (survives across launches, since it authenticates guest ssh access
//! independent of any one VM instance) plus per-launch artifacts (pidfile,
//! seed ISO, the three unix sockets krunkit bridges vsock onto, and the
//! serial log). Every path lives under the workspace config dir, never a
//! system temp dir, so `omnifs down`/`frontend down` can find and remove
//! exactly what this backend owns.
//!
//! Three vsock devices bridge the guest to the host, each on its own
//! `virtio-vsock` device (krunkit multiplexes by port, not by socket):
//! - port 1024 (attach): guest-initiated (`,listen`) onto the daemon's own
//!   vsock-attach unix socket (bound by `POST /v1/frontend/attach-target/vsock`;
//!   this backend never creates or removes that socket).
//! - port 1025 (ready): guest-initiated (`,listen`) onto a unix socket this
//!   backend binds and accepts on in a loop before spawning krunkit — a
//!   `,listen` device requires the host side already listening, since krunkit
//!   dials it once per guest connection rather than the reverse.
//! - port 22 (ssh): host-initiated (`,connect`, krunkit's explicit
//!   host-to-guest mode; omitting both keywords means guest-initiated):
//!   krunkit itself creates and listens on the unix socket, relaying each
//!   accepted connection into the guest's vsock-listening dropbear
//!   (`ListenStream=vsock::22` in the guest image). `omnifs shell` dials it
//!   through `ssh -o ProxyCommand='socat - UNIX-CONNECT:<path>'`.
//!
//! No `virtio-net` device is ever configured: the frontend carries no
//! credentials and needs no egress, so it gets no network authority at all.
//! [`assert_krunkit_locked_down`] verifies this against the live process's
//! argv immediately after spawn, mirroring the Docker backend's
//! `assert_locked_down`.

use std::io::{Read as _, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result};

use crate::config::Config;
use crate::frontend_backend::{FrontendBackend, FrontendLaunchSpec};
use crate::launch_backend::{BUILD_CHANNEL, BuildChannel, GUEST_MOUNT, ImageRef};

const KRUNKIT_SUBDIR: &str = "krunkit";
const SSH_KEY_NAME: &str = "id_ed25519";
const PIDFILE_NAME: &str = "krunkit.pid";
const SEED_ISO_NAME: &str = "seed.iso";
const SEED_STAGING_NAME: &str = "seed-staging";
const SSH_SOCK_NAME: &str = "ssh.sock";
const READY_SOCK_NAME: &str = "ready.sock";
const RESTFUL_SOCK_NAME: &str = "restful.sock";
const SERIAL_LOG_NAME: &str = "serial.log";

/// Guest vsock port the daemon's attach listener is proxied onto.
const ATTACH_VSOCK_PORT: u32 = 1024;
/// Guest vsock port the readiness beacon (see `omnifs-daemon/src/frontend.rs`)
/// dials once the FUSE mount is serving.
const READY_VSOCK_PORT: u32 = 1025;
/// Guest vsock port the image's socket-activated dropbear listens on.
const SSH_VSOCK_PORT: u32 = 22;

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

/// Where the krunkit driver's guest disk image comes from, gated purely by
/// [`BuildChannel`] (never by the shape of an override string): a dev binary
/// never downloads, so its resolution always yields [`Self::Local`], even
/// for an explicit override; a release binary always pulls from ghcr, so its
/// resolution always yields [`Self::Registry`]. See
/// `crate::guest_image_pull` for the [`Self::Registry`] pull-and-cache path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GuestImageSource {
    Local(PathBuf),
    Registry(ImageRef),
}

impl GuestImageSource {
    /// Resolve the configured source, then turn it into the local disk path
    /// krunkit needs. Dev images are already local; release images are pulled
    /// into the workspace cache on first use.
    pub(crate) fn resolve(image: Option<String>, config: &Config) -> Result<Self> {
        let resolved = crate::config::resolve_setting(
            image,
            ENV_GUEST_IMAGE,
            || config.frontend.guest_image.clone(),
            default_guest_image_for(BUILD_CHANNEL).to_string(),
        );
        match BUILD_CHANNEL {
            BuildChannel::Dev => Ok(Self::Local(PathBuf::from(resolved))),
            BuildChannel::Release => Ok(Self::Registry(ImageRef::new(resolved)?)),
        }
    }

    pub(crate) async fn into_local_path(self, cache_dir: &Path) -> Result<PathBuf> {
        match self {
            Self::Local(path) => Ok(path),
            Self::Registry(image) => {
                crate::guest_image_pull::ensure_guest_image(&image, cache_dir).await
            },
        }
    }
}

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

/// `--device` count a correctly locked-down krunkit process must carry:
/// root disk, seed disk, attach vsock, ready vsock, ssh vsock, serial log.
const EXPECTED_DEVICE_COUNT: usize = 6;

/// Conservative `sockaddr_un.sun_path` byte budget, mirroring
/// `omnifs-daemon`'s `check_uds_path_length` (kept as its own copy here: the
/// CLI and daemon do not share a path-validation crate, and this check is
/// small enough that a shared abstraction would cost more than it saves).
const UDS_PATH_BYTE_LIMIT: usize = 100;

fn check_uds_path_length(path: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt as _;
    let len = path.as_os_str().as_bytes().len();
    anyhow::ensure!(
        len < UDS_PATH_BYTE_LIMIT,
        "krunkit socket path {} is {len} bytes, at or beyond the {UDS_PATH_BYTE_LIMIT}-byte \
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

/// A truthful, actionable error naming the install command, rather than a
/// bare "command not found" surfaced from the failed spawn.
fn ensure_krunkit_available() -> Result<()> {
    match Command::new("krunkit")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => anyhow::bail!(
            "krunkit is not installed; install it with `brew tap slp/krun && brew install krunkit`"
        ),
        Err(error) => Err(error).context("probe for krunkit on PATH"),
    }
}

/// `omnifs shell`'s krunkit dispatch calls this before building the ssh
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
            "socat is required to reach the krunkit guest over vsock; install it with `brew install socat`"
        ),
        Err(error) => Err(error).context("probe for socat on PATH"),
    }
}

fn process_alive(pid: u32) -> bool {
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// The libkrun microVM frontend backend. Instance state is workspace-scoped;
/// launch-only inputs such as the guest image live in `FrontendLaunchSpec`.
pub(crate) struct KrunkitBackend {
    home: PathBuf,
    /// Flipped by the readiness accept-loop thread `launch` spawns, once the
    /// guest's frontend runner has dialed in with its `ready\n` line.
    /// `Arc` so the loop thread (which outlives the async `launch` call) and
    /// later `mount_ready` polls share the same flag within one CLI process
    /// invocation.
    ready: Arc<AtomicBool>,
}

impl KrunkitBackend {
    pub(crate) fn new(home: PathBuf) -> Self {
        Self {
            home,
            ready: Arc::new(AtomicBool::new(false)),
        }
    }

    fn dir(&self) -> PathBuf {
        self.home.join(KRUNKIT_SUBDIR)
    }

    fn ssh_key_path(&self) -> PathBuf {
        self.dir().join(SSH_KEY_NAME)
    }

    fn ssh_pubkey_path(&self) -> PathBuf {
        self.dir().join(format!("{SSH_KEY_NAME}.pub"))
    }

    fn pidfile(&self) -> PathBuf {
        self.dir().join(PIDFILE_NAME)
    }

    fn seed_iso(&self) -> PathBuf {
        self.dir().join(SEED_ISO_NAME)
    }

    fn seed_staging(&self) -> PathBuf {
        self.dir().join(SEED_STAGING_NAME)
    }

    fn ssh_socket(&self) -> PathBuf {
        self.dir().join(SSH_SOCK_NAME)
    }

    fn ready_socket(&self) -> PathBuf {
        self.dir().join(READY_SOCK_NAME)
    }

    fn restful_socket(&self) -> PathBuf {
        self.dir().join(RESTFUL_SOCK_NAME)
    }

    fn serial_log(&self) -> PathBuf {
        self.dir().join(SERIAL_LOG_NAME)
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
                .arg("omnifs-krunkit")
                .arg("-f")
                .arg(&key)
                .arg("-q")
                .status()
                .context("run ssh-keygen to generate the krunkit guest keypair")?;
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
        let status = Command::new("hdiutil")
            .arg("makehybrid")
            .arg("-iso")
            .arg("-joliet")
            .arg("-default-volume-name")
            .arg(SEED_VOLUME_LABEL)
            .arg("-o")
            .arg(&out)
            .arg(&staging)
            .stdout(Stdio::null())
            .status()
            .context("run hdiutil makehybrid to build the seed ISO")?;
        anyhow::ensure!(status.success(), "hdiutil makehybrid exited with {status}");

        let _ = std::fs::remove_dir_all(&staging);
        Ok(())
    }

    /// Bind the readiness unix socket and spawn a background thread that
    /// accepts on it in a loop, flipping `self.ready` on the first `ready`
    /// line. A `,listen` vsock device requires the host side already
    /// listening (krunkit dials it once per guest connection), and a
    /// one-shot listener would die after the first connection, so the loop
    /// keeps accepting for the process lifetime.
    fn spawn_ready_accept_loop(&self) -> Result<()> {
        let path = self.ready_socket();
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)
            .with_context(|| format!("bind readiness listener {}", path.display()))?;
        let flag = Arc::clone(&self.ready);
        std::thread::spawn(move || {
            loop {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                let mut buf = [0_u8; 64];
                if let Ok(n) = stream.read(&mut buf)
                    && buf[..n].starts_with(b"ready")
                {
                    flag.store(true, Ordering::SeqCst);
                }
            }
        });
        Ok(())
    }

    async fn wait_for_pidfile(&self) -> Result<u32> {
        let pidfile = self.pidfile();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(contents) = std::fs::read_to_string(&pidfile)
                && let Ok(pid) = contents.trim().parse::<u32>()
            {
                return Ok(pid);
            }
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "krunkit did not write its pidfile at {} within 5s",
                pidfile.display()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    fn read_pidfile(&self) -> Result<Option<u32>> {
        match std::fs::read_to_string(self.pidfile()) {
            Ok(contents) => Ok(contents.trim().parse::<u32>().ok()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).context("read krunkit pidfile"),
        }
    }

    /// Read back the live process's argv via `ps` (macOS has no `/proc`) and
    /// assert it against the exact device set `launch` demands. Kills and
    /// cleans up on violation rather than reporting success.
    fn probe_argv(pid: u32) -> Result<String> {
        let output = Command::new("ps")
            .args(["-ww", "-p", &pid.to_string(), "-o", "command="])
            .output()
            .context("probe live krunkit argv via ps")?;
        anyhow::ensure!(
            output.status.success() && !output.stdout.is_empty(),
            "ps could not find krunkit pid {pid} right after spawn"
        );
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Best-effort restful shutdown request. Failures (unreachable socket,
    /// unexpected response) are swallowed: `tear_down` falls through to
    /// SIGTERM/SIGKILL regardless, and the exact restful shutdown API shape
    /// is not independently confirmed in this build (see the krunkit
    /// contract note in `docs/contracts/40-frontends.md`).
    fn try_restful_shutdown(&self) {
        let Ok(mut stream) = UnixStream::connect(self.restful_socket()) else {
            return;
        };
        let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
        let body = br#"{"state":"Stop"}"#;
        let request = format!(
            "POST /vm/state HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(request.as_bytes());
        let _ = stream.write_all(body);
    }

    async fn wait_for_exit(pid: u32, deadline: Duration) -> bool {
        let until = tokio::time::Instant::now() + deadline;
        while process_alive(pid) {
            if tokio::time::Instant::now() >= until {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        true
    }

    /// Remove every artifact this backend owns. Never touches the daemon's
    /// own vsock-attach socket: that path is not stored on this struct at
    /// all, only threaded through `launch` from the caller's spec.
    fn remove_owned_artifacts(&self) {
        for path in [
            self.pidfile(),
            self.seed_iso(),
            self.ssh_socket(),
            self.ready_socket(),
            self.restful_socket(),
            self.serial_log(),
        ] {
            let _ = std::fs::remove_file(path);
        }
        let _ = std::fs::remove_dir_all(self.seed_staging());
    }
}

/// Bounds the seed staging dir to exactly the expected `KEY=VALUE` lines
/// before it is burned into an ISO the guest can read: one file, the exact
/// key set, no duplicates. This is the seed half of the launch-time lockdown
/// audit; [`assert_krunkit_locked_down`] is the device-set half.
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

/// Assert the live krunkit process's argv carries exactly the device set the
/// spec demands: no `virtio-net`, exactly two `virtio-blk` (root + seed), the
/// three expected `virtio-vsock` devices at their expected socket paths, the
/// serial log, `--restful-uri`, and `--pidfile`. A pure function of the argv
/// string so it is unit-testable without spawning a real krunkit process.
fn assert_krunkit_locked_down(
    argv: &str,
    attach_socket: &Path,
    ready_socket: &Path,
    ssh_socket: &Path,
) -> Result<(), String> {
    if argv.contains("virtio-net") {
        return Err(
            "the process carries a virtio-net device; the frontend must have no network authority"
                .to_string(),
        );
    }
    let device_count = argv.matches("--device").count();
    if device_count != EXPECTED_DEVICE_COUNT {
        return Err(format!(
            "the process has {device_count} --device flag(s), expected exactly \
             {EXPECTED_DEVICE_COUNT} (root disk, seed disk, attach/ready/ssh vsock, serial log)"
        ));
    }
    let blk_count = argv.matches("virtio-blk,path=").count();
    if blk_count != 2 {
        return Err(format!(
            "expected exactly 2 virtio-blk devices (root + seed), found {blk_count}"
        ));
    }
    let expected_attach = format!(
        "virtio-vsock,port={ATTACH_VSOCK_PORT},socketURL={},listen",
        attach_socket.display()
    );
    if !argv.contains(&expected_attach) {
        return Err("missing the expected attach vsock device".to_string());
    }
    let expected_ready = format!(
        "virtio-vsock,port={READY_VSOCK_PORT},socketURL={},listen",
        ready_socket.display()
    );
    if !argv.contains(&expected_ready) {
        return Err("missing the expected readiness vsock device".to_string());
    }
    let expected_ssh = format!(
        "virtio-vsock,port={SSH_VSOCK_PORT},socketURL={},connect",
        ssh_socket.display()
    );
    if !argv.contains(&expected_ssh) {
        return Err("missing the expected ssh vsock device".to_string());
    }
    if !argv.contains("virtio-serial,logFilePath=") {
        return Err("missing the expected virtio-serial log device".to_string());
    }
    if !argv.contains("--restful-uri") {
        return Err("missing --restful-uri".to_string());
    }
    if !argv.contains("--pidfile") {
        return Err("missing --pidfile".to_string());
    }
    Ok(())
}

impl FrontendBackend for KrunkitBackend {
    async fn launch(&self, spec: &FrontendLaunchSpec) -> Result<()> {
        ensure_krunkit_available()?;
        let FrontendLaunchSpec::Krunkit {
            attach_socket: daemon_attach_socket,
            attach_token,
            guest_image,
        } = spec
        else {
            anyhow::bail!("internal: the krunkit backend received a docker launch spec");
        };
        anyhow::ensure!(
            guest_image.is_file(),
            "guest image not found at {}; build it with `just guest-image` \
             (see docs/contracts/60-build-validation.md)",
            guest_image.display()
        );

        // Replace any prior instance before laying down fresh launch state.
        self.tear_down()
            .await
            .context("tear down a prior krunkit instance before relaunch")?;

        let dir = self.dir();
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restrict {} to 0700", dir.display()))?;

        for path in [
            daemon_attach_socket.as_path(),
            self.ready_socket().as_path(),
            self.ssh_socket().as_path(),
            self.restful_socket().as_path(),
        ] {
            check_uds_path_length(path)?;
        }

        let ssh_pubkey = self.ensure_ssh_keypair()?;
        self.write_seed_iso(attach_token, &ssh_pubkey)?;
        self.spawn_ready_accept_loop()?;
        let _ = std::fs::remove_file(self.ssh_socket());

        let pidfile = self.pidfile();
        let devices = [
            format!("virtio-blk,path={},format=raw", guest_image.display()),
            format!("virtio-blk,path={},format=raw", self.seed_iso().display()),
            format!(
                "virtio-vsock,port={ATTACH_VSOCK_PORT},socketURL={},listen",
                daemon_attach_socket.display()
            ),
            format!(
                "virtio-vsock,port={READY_VSOCK_PORT},socketURL={},listen",
                self.ready_socket().display()
            ),
            format!(
                "virtio-vsock,port={SSH_VSOCK_PORT},socketURL={},connect",
                self.ssh_socket().display()
            ),
            format!("virtio-serial,logFilePath={}", self.serial_log().display()),
        ];
        let mut command = Command::new("krunkit");
        command.args(["--cpus", "2", "--memory", "2048"]);
        for device in devices {
            command.arg("--device").arg(device);
        }
        command
            .arg("--restful-uri")
            .arg(format!("unix://{}", self.restful_socket().display()))
            .arg("--pidfile")
            .arg(&pidfile)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command.process_group(0);
        }

        // Detached: the VM outlives this CLI invocation. Do not hold or wait
        // on the child handle; teardown reads the pidfile instead, mirroring
        // the host-native daemon's own detached-spawn pattern
        // (`launch_backend::launch_native`).
        command.spawn().context("spawn krunkit")?;

        let pid = self.wait_for_pidfile().await?;
        let argv = Self::probe_argv(pid)?;
        if let Err(violation) = assert_krunkit_locked_down(
            &argv,
            daemon_attach_socket,
            &self.ready_socket(),
            &self.ssh_socket(),
        ) {
            let _ = Command::new("kill")
                .arg("-KILL")
                .arg(pid.to_string())
                .status();
            self.remove_owned_artifacts();
            anyhow::bail!("refusing to run the krunkit VM: {violation}");
        }

        Ok(())
    }

    async fn mount_ready(&self, _path: &str) -> Result<bool> {
        // Krunkit has no docker-exec-equivalent channel to probe a specific
        // guest path from outside the VM; the readiness beacon
        // (`crates/omnifs-daemon/src/frontend.rs`) already gates on the FUSE
        // mount being served before it dials in, so observing that beacon is
        // the whole-guest equivalent of Docker's per-path probe.
        Ok(self.ready.load(Ordering::SeqCst))
    }

    async fn is_running(&self) -> Result<Option<bool>> {
        let Some(pid) = self.read_pidfile()? else {
            return Ok(None);
        };
        Ok(Some(process_alive(pid)))
    }

    async fn tear_down(&self) -> Result<()> {
        if let Some(pid) = self.read_pidfile()?
            && process_alive(pid)
        {
            self.try_restful_shutdown();
            if !Self::wait_for_exit(pid, Duration::from_secs(5)).await {
                let _ = Command::new("kill")
                    .arg("-TERM")
                    .arg(pid.to_string())
                    .status();
            }
            if !Self::wait_for_exit(pid, Duration::from_secs(5)).await && process_alive(pid) {
                let _ = Command::new("kill")
                    .arg("-KILL")
                    .arg(pid.to_string())
                    .status();
                Self::wait_for_exit(pid, Duration::from_secs(3)).await;
            }
        }
        self.remove_owned_artifacts();
        Ok(())
    }

    /// Pure command construction: no I/O. Callers that are about to actually
    /// run this must probe for `socat` themselves (`ensure_socat_available`).
    fn shell_command(&self, shell_override: Option<&str>, trailing: &[String]) -> Command {
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

    #[test]
    fn guest_image_resolution_precedence() {
        // Tests run under a dev build (no OMNIFS_RELEASE at compile time), so
        // BUILD_CHANNEL is always Dev here; the release branch is covered by
        // `default_guest_image_for` directly, mirroring how
        // `frontend_container.rs` tests its own two channel defaults.
        let config = Config::default();
        let image = GuestImageSource::resolve(None, &config).unwrap();
        assert_eq!(
            image,
            GuestImageSource::Local(PathBuf::from(DEFAULT_GUEST_IMAGE))
        );

        let flag =
            GuestImageSource::resolve(Some("/custom/guest.raw".to_string()), &config).unwrap();
        assert_eq!(
            flag,
            GuestImageSource::Local(PathBuf::from("/custom/guest.raw"))
        );
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

    fn sample_argv() -> &'static str {
        "krunkit --cpus 2 --memory 2048 \
         --device virtio-blk,path=/img/root.raw,format=raw \
         --device virtio-blk,path=/img/seed.iso,format=raw \
         --device virtio-vsock,port=1024,socketURL=/h/attach.sock,listen \
         --device virtio-vsock,port=1025,socketURL=/h/ready.sock,listen \
         --device virtio-vsock,port=22,socketURL=/h/ssh.sock,connect \
         --device virtio-serial,logFilePath=/h/serial.log \
         --restful-uri unix:///h/restful.sock --pidfile /h/krunkit.pid"
    }

    #[test]
    fn lockdown_accepts_the_exact_expected_device_set() {
        assert_krunkit_locked_down(
            sample_argv(),
            Path::new("/h/attach.sock"),
            Path::new("/h/ready.sock"),
            Path::new("/h/ssh.sock"),
        )
        .expect("the exact expected device set must pass");
    }

    #[test]
    fn lockdown_rejects_a_virtio_net_device() {
        let argv = format!(
            "{} --device virtio-net,unixSocketPath=/h/net.sock",
            sample_argv()
        );
        let err = assert_krunkit_locked_down(
            &argv,
            Path::new("/h/attach.sock"),
            Path::new("/h/ready.sock"),
            Path::new("/h/ssh.sock"),
        )
        .unwrap_err();
        assert!(err.contains("virtio-net"));
    }

    #[test]
    fn lockdown_rejects_an_unexpected_attach_socket() {
        let err = assert_krunkit_locked_down(
            sample_argv(),
            Path::new("/h/wrong-attach.sock"),
            Path::new("/h/ready.sock"),
            Path::new("/h/ssh.sock"),
        )
        .unwrap_err();
        assert!(err.contains("attach"));
    }

    #[test]
    fn lockdown_rejects_a_missing_device() {
        let argv = sample_argv().replace("--device virtio-serial,logFilePath=/h/serial.log", "");
        let err = assert_krunkit_locked_down(
            &argv,
            Path::new("/h/attach.sock"),
            Path::new("/h/ready.sock"),
            Path::new("/h/ssh.sock"),
        )
        .unwrap_err();
        assert!(err.contains("--device flag"));
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
