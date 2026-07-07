//! Frontend conformance matrix: the product-contract toolbox run against a live
//! omnifs mount, once per frontend column, emitting a JSON scorecard and a
//! rendered markdown table per lane.
//!
//! Supersedes the single-purpose `omnifs-cli` `frontend_conformance` test. The
//! scorecards are the evidence base for the default-runtime and write-path
//! decisions, so every lane always writes its scorecard and prints its table
//! before asserting — a red run still leaves evidence.
//!
//! Lanes:
//! - `native_frontend_matrix` (env `OMNIFS_ACCEPTANCE_LIVE`): the daemon with
//!   the platform-default host-native frontend (kernel FUSE on Linux, `NFSv4`
//!   loopback on macOS). Column is cfg-selected.
//! - `fuse_in_docker_matrix` (env `OMNIFS_ACCEPTANCE_DOCKER`): the daemon plus
//!   FUSE inside the product runtime container; rows run through `docker exec`.

#![cfg(not(target_os = "wasi"))]

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use omnifs_itest::live;
use omnifs_itest::matrix::{self, Column, Exec, ROWS};

/// The platform-default native column for this OS.
#[cfg(target_os = "linux")]
const NATIVE_COLUMN: &Column = &matrix::LINUX_FUSE_NATIVE;
#[cfg(target_os = "macos")]
const NATIVE_COLUMN: &Column = &matrix::MACOS_NFS_LOOPBACK;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
const NATIVE_COLUMN: &Column = &matrix::LINUX_FUSE_NATIVE;

/// Write the scorecard and print the table, then assert every row matched its
/// expectation. Evidence lands before the assertion so a red run is diagnosable.
fn finish(scorecard: &matrix::Scorecard) {
    let path = matrix::write_scorecard(scorecard);
    let table = matrix::render_table(std::slice::from_ref(scorecard));
    eprintln!("scorecard: {}", path.display());
    eprintln!("\n{table}");
    let mismatches = matrix::mismatches(scorecard);
    assert!(
        mismatches.is_empty(),
        "frontend matrix column `{}` has {} expectation mismatch(es):\n  {}",
        scorecard.column,
        mismatches.len(),
        mismatches.join("\n  ")
    );
}

#[test]
fn native_frontend_matrix() {
    if std::env::var_os("OMNIFS_ACCEPTANCE_LIVE").is_none() {
        eprintln!("skip: set OMNIFS_ACCEPTANCE_LIVE=1 to run live-mount acceptance tests");
        return;
    }
    let Some(daemon) = live::start_native_daemon() else {
        return;
    };

    let scratch = tempfile::tempdir().expect("scratch dir");
    let exec = Exec::Local {
        root: daemon.tree_root(),
        scratch: scratch.path().to_path_buf(),
    };

    let scorecard = matrix::run_column(&exec, NATIVE_COLUMN, ROWS);
    finish(&scorecard);
}

#[test]
fn fuse_in_docker_matrix() {
    if std::env::var_os("OMNIFS_ACCEPTANCE_DOCKER").is_none() {
        eprintln!("skip: set OMNIFS_ACCEPTANCE_DOCKER=1 to run the fuse-in-docker lane");
        return;
    }
    let Some(image) = select_dev_image() else {
        eprintln!(
            "skip: no local `omnifs:*dev` image found; run `just dev` to build the runtime image"
        );
        return;
    };

    let hermetic = live::hermetic_home();
    let container_name = format!("omnifs-matrix-{}", std::process::id());
    let Some(container) = DockerContainer::start(&container_name, &image, hermetic.home.path())
    else {
        return;
    };

    let exec = Exec::DockerExec {
        container: container.name.clone(),
        root: "/omnifs/test".to_string(),
        scratch: DOCKER_SCRATCH.to_string(),
    };

    let scorecard = matrix::run_column(&exec, &matrix::FUSE_IN_DOCKER, ROWS);
    // `container` and `hermetic` drop after `finish`, tearing down the mount.
    finish(&scorecard);
    drop(container);
    drop(hermetic);
}

const DOCKER_SCRATCH: &str = "/tmp/omnifs-matrix";

/// Pick the runtime image for the docker lane: the floating `omnifs:dev` tag if
/// present, else the newest local image whose tag contains `dev` (content-tagged
/// dev builds like `omnifs:<hash>-dev`).
fn select_dev_image() -> Option<String> {
    if docker(&["image", "inspect", "omnifs:dev"]).is_some() {
        return Some("omnifs:dev".to_string());
    }
    // `docker images` lists newest first, so the first dev-tagged match wins.
    let listing = docker(&["images", "omnifs", "--format", "{{.Repository}}:{{.Tag}}"])?;
    listing
        .lines()
        .map(str::trim)
        .find(|tag| tag.contains("dev") && !tag.ends_with(":<none>"))
        .map(str::to_string)
}

/// Run a docker command, returning its trimmed stdout on success, `None`
/// otherwise (docker missing, non-zero exit).
fn docker(args: &[&str]) -> Option<String> {
    let output = Command::new("docker").args(args).output().ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

/// A running product runtime container, force-removed on drop.
struct DockerContainer {
    name: String,
}

impl Drop for DockerContainer {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "-f", &self.name])
            .output();
    }
}

impl DockerContainer {
    /// Launch the runtime image replicating the product launcher's container
    /// contract: home bind mount, `/dev/fuse`, `SYS_ADMIN`, apparmor unconfined.
    /// The image entrypoint starts `omnifs daemon` and mounts `/omnifs`.
    ///
    /// Returns `None` (skip) if docker cannot run the container or the mount
    /// never serves; the caller has already gated on `OMNIFS_ACCEPTANCE_DOCKER`.
    fn start(name: &str, image: &str, host_home: &Path) -> Option<Self> {
        // Clear any stale container from a previous interrupted run.
        let _ = Command::new("docker").args(["rm", "-f", name]).output();

        let bind = format!("{}:/root/.omnifs", host_home.display());
        let run = Command::new("docker")
            .args([
                "run",
                "-d",
                "--name",
                name,
                "--device",
                "/dev/fuse",
                "--cap-add",
                "SYS_ADMIN",
                "--security-opt",
                "apparmor:unconfined",
                "-v",
                &bind,
                image,
            ])
            .output()
            .ok()?;
        if !run.status.success() {
            eprintln!(
                "skip: docker run failed: {}",
                String::from_utf8_lossy(&run.stderr).trim()
            );
            return None;
        }

        let container = Self {
            name: name.to_string(),
        };

        // Wait for the FUSE mount to serve the projected tree inside the
        // container, bailing if the container exits first.
        let deadline = Instant::now() + Duration::from_mins(1);
        loop {
            let served = Command::new("docker")
                .args(["exec", name, "test", "-f", "/omnifs/test/hello/message"])
                .output()
                .is_ok_and(|o| o.status.success());
            if served {
                break;
            }
            let running = docker(&["inspect", "-f", "{{.State.Running}}", name])
                .is_some_and(|state| state.trim() == "true");
            if !running {
                let logs = docker(&["logs", "--tail", "40", name]).unwrap_or_default();
                eprintln!("skip: container `{name}` exited before serving /omnifs\n{logs}");
                return None;
            }
            if Instant::now() >= deadline {
                let logs = docker(&["logs", "--tail", "40", name]).unwrap_or_default();
                eprintln!("skip: /omnifs/test/hello/message never appeared within 60s\n{logs}");
                return None;
            }
            std::thread::sleep(Duration::from_millis(500));
        }

        // Provide a writable scratch dir inside the container for the rows.
        let _ = Command::new("docker")
            .args(["exec", name, "mkdir", "-p", DOCKER_SCRATCH])
            .output();

        Some(container)
    }
}
