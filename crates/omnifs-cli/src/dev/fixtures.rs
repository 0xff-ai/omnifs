//! CLI-owned fixtures for `omnifs dev`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context as _, Result, bail};

const DB_IMAGE: &str = "omnifs-dev-db:local";
const DB_CONTAINER: &str = "omnifs-dev-db";
const K8S_COMPOSE_PROJECT: &str = "omnifs-devcluster";
const GUEST_DB_DIR: &str = "/data";
const GUEST_SOCK_DIR: &str = "/run/omnifs";

/// Running fixture containers. Teardown is explicit via [`FixtureSession::down`]
/// or [`DevSessionRecord::teardown_all`]; detached sessions keep fixtures alive
/// until `omnifs down`.
pub(crate) struct FixtureSession {
    workspace: PathBuf,
    db_container_id: Option<String>,
    k8s: bool,
    k8s_sock_dir: Option<PathBuf>,
}

impl FixtureSession {
    /// Bring up fixtures required by profile mount names.
    pub(crate) fn up(
        profile_mounts: &[String],
        dev_home: &Path,
        workspace: &Path,
    ) -> Result<(Self, Vec<String>)> {
        let wants_db = profile_mounts.iter().any(|name| name == "db");
        let wants_k8s = profile_mounts.iter().any(|name| name == "k8s");

        let mut binds: Vec<String> = Vec::new();
        let mut db_container_id = None;
        let mut k8s = false;
        let mut k8s_sock_dir = None;

        if wants_db {
            let db_dir = dev_home.join("fixtures/db");
            std::fs::create_dir_all(&db_dir)
                .with_context(|| format!("create {}", db_dir.display()))?;
            db_container_id = Some(start_db_fixture(workspace, &db_dir)?);
            binds.push(format!("{}:{GUEST_DB_DIR}:ro", db_dir.display()));
        }

        if wants_k8s {
            let sock_dir = dev_home.join("fixtures/k8s");
            std::fs::create_dir_all(&sock_dir)
                .with_context(|| format!("create {}", sock_dir.display()))?;
            start_k8s_fixture(workspace, &sock_dir)?;
            k8s = true;
            k8s_sock_dir = Some(sock_dir.clone());
            binds.push(format!("{}:{GUEST_SOCK_DIR}", sock_dir.display()));
        }

        Ok((
            Self {
                workspace: workspace.to_path_buf(),
                db_container_id,
                k8s,
                k8s_sock_dir,
            },
            binds,
        ))
    }

    pub(crate) fn db_container_id(&self) -> Option<String> {
        self.db_container_id.clone()
    }

    pub(crate) fn k8s_active(&self) -> bool {
        self.k8s
    }

    pub(crate) fn k8s_sock_dir(&self) -> Option<&Path> {
        self.k8s_sock_dir.as_deref()
    }

    /// Best-effort teardown of any running fixtures using live handles.
    pub(crate) fn down(self) -> Result<()> {
        if self.k8s {
            let sock_dir = self
                .k8s_sock_dir
                .as_deref()
                .unwrap_or_else(|| Path::new("/tmp/omnifs-k8s-sock"));
            teardown_k8s_compose(&self.workspace, sock_dir)?;
        }
        if let Some(container_id) = &self.db_container_id {
            remove_container(container_id, "db fixture container")?;
        }
        Ok(())
    }
}

/// Remove `dev_home/<name>` if present, tolerating an already-absent file.
fn remove_file_if_present(dev_home: &Path, name: &str) -> Result<()> {
    let path = dev_home.join(name);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}

fn start_db_fixture(workspace: &Path, db_dir: &Path) -> Result<String> {
    let context = workspace.join("providers/db/dev");
    let dockerfile = context.join("Dockerfile");
    if !dockerfile.is_file() {
        bail!(
            "db fixture Dockerfile not found at {}",
            dockerfile.display()
        );
    }

    tracing::info!("building db fixture image (Chinook SQLite)");
    run_status(
        Command::new("docker")
            .args(["build", "-t", DB_IMAGE, "."])
            .current_dir(&context),
        "build db fixture image",
    )?;

    let _ = remove_container(DB_CONTAINER, "stale db fixture container");

    tracing::info!("starting db fixture container");
    let output = Command::new("docker")
        .args([
            "run",
            "-d",
            "--name",
            DB_CONTAINER,
            "-v",
            &format!("{}:/data", db_dir.display()),
            DB_IMAGE,
        ])
        .output()
        .context("start db fixture container")?;
    if !output.status.success() {
        bail!(
            "start db fixture container failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn start_k8s_fixture(workspace: &Path, sock_dir: &Path) -> Result<()> {
    tracing::info!("starting dev Kubernetes cluster (k3s)");
    let compose_file = k8s_compose_file(workspace)?;
    run_status(
        docker_compose(&compose_file, sock_dir).args(["up", "-d", "--wait"]),
        "start k8s fixture compose stack",
    )
}

fn teardown_k8s_compose(workspace: &Path, sock_dir: &Path) -> Result<()> {
    let compose_file = k8s_compose_file(workspace)?;
    run_status(
        docker_compose(&compose_file, sock_dir).args(["down", "-v"]),
        "stop k8s fixture compose stack",
    )
}

fn k8s_compose_file(workspace: &Path) -> Result<PathBuf> {
    let compose_file = workspace.join("providers/kubernetes/dev/compose.yaml");
    if !compose_file.is_file() {
        bail!(
            "kubernetes dev compose file not found at {}",
            compose_file.display()
        );
    }
    Ok(compose_file)
}

fn docker_compose(compose_file: &Path, sock_dir: &Path) -> Command {
    let mut command = Command::new("docker");
    command
        .args([
            "compose",
            "-p",
            K8S_COMPOSE_PROJECT,
            "-f",
            &compose_file.display().to_string(),
        ])
        .env("OMNIFS_K8S_SOCK_DIR", sock_dir);
    command
}

fn remove_container(container: &str, context: &str) -> Result<()> {
    let status = Command::new("docker")
        .args(["rm", "-f", container])
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("remove {context} `{container}`"))?;
    if !status.success() {
        tracing::debug!("{context} `{container}` removal exited with {status}");
    }
    Ok(())
}

fn run_status(command: &mut Command, context: &str) -> Result<()> {
    let status = command.status().with_context(|| context.to_string())?;
    if !status.success() {
        bail!("{context} failed");
    }
    Ok(())
}

fn teardown_fixtures(record: &DevSessionRecord) {
    if record.fixtures.k8s {
        let sock_dir = record
            .fixtures
            .k8s_sock_dir
            .as_deref()
            .unwrap_or_else(|| Path::new("/tmp/omnifs-k8s-sock"));
        if let Err(error) = teardown_k8s_compose(&record.workspace, sock_dir) {
            tracing::debug!("k8s fixture teardown: {error:#}");
        }
    }

    if let Some(container_id) = &record.fixtures.db_container_id
        && let Err(error) = remove_container(container_id, "db fixture container")
    {
        tracing::debug!("db fixture container `{container_id}` teardown: {error:#}");
    }
}

/// Minimal dev session state for crash recovery and `omnifs down` orphan sweep.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub(crate) struct DevSessionRecord {
    pub(crate) workspace: PathBuf,
    pub(crate) profile: String,
    pub(crate) container_name: String,
    pub(crate) fixtures: DevSessionFixtures,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub(crate) struct DevSessionFixtures {
    pub(crate) k8s: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) k8s_sock_dir: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) db_container_id: Option<String>,
}

impl DevSessionRecord {
    pub(crate) fn read(dev_home: &Path) -> Result<Option<Self>> {
        let path = dev_home.join("session.json");
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("read {}", path.display()));
            },
        };
        let session = serde_json::from_str(&raw).context("parse dev session record")?;
        Ok(Some(session))
    }

    pub(crate) fn write(&self, dev_home: &Path) -> Result<()> {
        let path = dev_home.join("session.json");
        let json = serde_json::to_string_pretty(self).context("serialize dev session")?;
        std::fs::write(&path, format!("{json}\n"))
            .with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }

    pub(crate) fn clear(dev_home: &Path) -> Result<()> {
        remove_file_if_present(dev_home, "session.json")
    }

    pub(crate) fn clear_launch_record(dev_home: &Path) -> Result<()> {
        remove_file_if_present(dev_home, "launch.json")
    }

    /// Full dev session teardown: runtime container, fixtures, session and launch records.
    pub(crate) fn teardown_all(dev_home: &Path) -> Result<()> {
        let Some(record) = Self::read(dev_home)? else {
            return Ok(());
        };

        if let Err(error) = remove_container(&record.container_name, "runtime container") {
            tracing::debug!("runtime container teardown: {error:#}");
        }

        teardown_fixtures(&record);

        let _ = Self::clear(dev_home);
        let _ = Self::clear_launch_record(dev_home);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_session_record_round_trip() {
        let record = DevSessionRecord {
            workspace: PathBuf::from("/tmp/workspace"),
            profile: "default".to_string(),
            container_name: "omnifs-dev".to_string(),
            fixtures: DevSessionFixtures {
                k8s: true,
                k8s_sock_dir: Some(PathBuf::from("/tmp/k8s-sock")),
                db_container_id: Some("abc123".to_string()),
            },
        };

        let dir = tempfile::tempdir().unwrap();
        record.write(dir.path()).unwrap();
        let read = DevSessionRecord::read(dir.path()).unwrap().unwrap();
        assert_eq!(read, record);
    }

    #[test]
    fn clear_session_record_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        DevSessionRecord::clear(dir.path()).unwrap();
        DevSessionRecord::clear(dir.path()).unwrap();
    }

    #[test]
    fn clear_launch_record_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        DevSessionRecord::clear_launch_record(dir.path()).unwrap();
        DevSessionRecord::clear_launch_record(dir.path()).unwrap();
    }

    #[test]
    fn teardown_all_without_session_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        DevSessionRecord::teardown_all(dir.path()).unwrap();
        DevSessionRecord::teardown_all(dir.path()).unwrap();
    }

    #[test]
    fn teardown_all_clears_session_files() {
        let dir = tempfile::tempdir().unwrap();
        let record = DevSessionRecord {
            workspace: dir.path().join("workspace"),
            profile: "smoke".to_string(),
            container_name: "omnifs-dev-missing".to_string(),
            fixtures: DevSessionFixtures::default(),
        };
        record.write(dir.path()).unwrap();
        std::fs::write(dir.path().join("launch.json"), "{}\n").unwrap();

        DevSessionRecord::teardown_all(dir.path()).unwrap();

        assert!(!dir.path().join("session.json").exists());
        assert!(!dir.path().join("launch.json").exists());
    }
}
