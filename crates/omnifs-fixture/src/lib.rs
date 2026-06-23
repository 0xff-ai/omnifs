//! Testcontainers-backed fixtures for the contributor dev session.
//!
//! Brings up optional provider fixtures (Chinook `SQLite`, local k3s) before the
//! omnifs runtime container starts, and tears them down when the session ends.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use bollard::Docker;
use bollard::query_parameters::RemoveContainerOptions;
use testcontainers::compose::DockerCompose;
use testcontainers::core::Mount;
use testcontainers::runners::{AsyncBuilder, AsyncRunner};
use testcontainers::{ContainerAsync, GenericBuildableImage, GenericImage, ImageExt};

const DB_IMAGE: &str = "omnifs-dev-db";
const DB_TAG: &str = "local";
const DB_CONTAINER: &str = "omnifs-dev-db";
const K8S_COMPOSE_PROJECT: &str = "omnifs-devcluster";
const GUEST_DB_DIR: &str = "/data";
const GUEST_SOCK_DIR: &str = "/run/omnifs";

/// A provider mount name from a dev profile (e.g. `"github"`, `"db"`, `"k8s"`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MountSpec {
    pub name: String,
}

/// Host bind strings to layer onto the omnifs runtime container.
#[derive(Debug, Clone, Default)]
pub struct FixtureBinds {
    pub binds: Vec<String>,
}

/// Running fixture containers. Teardown is explicit via [`FixtureSession::down`]
/// or [`DevSessionRecord::teardown_all`]; there is no `Drop` impl so detached
/// sessions keep fixtures alive until `omnifs down`.
pub struct FixtureSession {
    db: Option<ContainerAsync<GenericImage>>,
    k8s: Option<DockerCompose>,
    k8s_sock_dir: Option<PathBuf>,
}

impl FixtureSession {
    /// Bring up fixtures required by `profile_mounts`.
    pub async fn up(
        profile_mounts: &[MountSpec],
        dev_home: &Path,
        workspace: &Path,
    ) -> Result<(Self, FixtureBinds)> {
        let wants_db = profile_mounts.iter().any(|m| m.name == "db");
        let wants_k8s = profile_mounts.iter().any(|m| m.name == "k8s");

        let mut binds = FixtureBinds::default();
        let mut db = None;
        let mut k8s = None;
        let mut k8s_sock_dir = None;

        if wants_db {
            let db_dir = dev_home.join("fixtures/db");
            std::fs::create_dir_all(&db_dir)
                .with_context(|| format!("create {}", db_dir.display()))?;
            db = Some(start_db_fixture(workspace, &db_dir).await?);
            binds
                .binds
                .push(format!("{}:{GUEST_DB_DIR}:ro", db_dir.display()));
        }

        if wants_k8s {
            let sock_dir = dev_home.join("fixtures/k8s");
            std::fs::create_dir_all(&sock_dir)
                .with_context(|| format!("create {}", sock_dir.display()))?;
            k8s = Some(start_k8s_fixture(workspace, &sock_dir).await?);
            k8s_sock_dir = Some(sock_dir.clone());
            binds
                .binds
                .push(format!("{}:{GUEST_SOCK_DIR}", sock_dir.display()));
        }

        Ok((
            Self {
                db,
                k8s,
                k8s_sock_dir,
            },
            binds,
        ))
    }

    pub fn db_container_id(&self) -> Option<String> {
        self.db.as_ref().map(|container| container.id().to_string())
    }

    pub fn k8s_active(&self) -> bool {
        self.k8s.is_some()
    }

    pub fn k8s_sock_dir(&self) -> Option<&Path> {
        self.k8s_sock_dir.as_deref()
    }

    /// Best-effort teardown of any running fixtures using live handles.
    pub async fn down(self) -> Result<()> {
        let mut session = self;
        if let Some(compose) = session.k8s.take() {
            compose
                .down()
                .await
                .context("stop k8s fixture compose stack")?;
        }
        if let Some(container) = session.db.take() {
            container
                .stop()
                .await
                .context("stop db fixture container")?;
        }
        Ok(())
    }
}

async fn start_db_fixture(workspace: &Path, db_dir: &Path) -> Result<ContainerAsync<GenericImage>> {
    let context = workspace.join("providers/db/dev");
    let dockerfile = context.join("Dockerfile");
    if !dockerfile.is_file() {
        bail!(
            "db fixture Dockerfile not found at {}",
            dockerfile.display()
        );
    }

    tracing::info!("building db fixture image (Chinook SQLite)");
    let image = GenericBuildableImage::new(DB_IMAGE, DB_TAG)
        .with_dockerfile(&dockerfile)
        .with_file(context.join("seed-entrypoint.sh"), "./seed-entrypoint.sh")
        .build_image()
        .await
        .context("build db fixture image")?;

    tracing::info!("starting db fixture container");
    let container = image
        .with_container_name(DB_CONTAINER)
        .with_mount(Mount::bind_mount(
            db_dir.to_string_lossy(),
            "/data".to_string(),
        ))
        .start()
        .await
        .context("start db fixture container")?;
    Ok(container)
}

fn k8s_compose(workspace: &Path, sock_dir: &Path) -> Result<DockerCompose> {
    let compose_file = workspace.join("providers/kubernetes/dev/compose.yaml");
    if !compose_file.is_file() {
        bail!(
            "kubernetes dev compose file not found at {}",
            compose_file.display()
        );
    }

    let mut compose = DockerCompose::with_local_client(&[compose_file])
        .with_project_name(K8S_COMPOSE_PROJECT)
        .with_env(
            "OMNIFS_K8S_SOCK_DIR",
            sock_dir.to_string_lossy().into_owned(),
        );
    compose.with_remove_volumes(true);
    Ok(compose)
}

async fn start_k8s_fixture(workspace: &Path, sock_dir: &Path) -> Result<DockerCompose> {
    tracing::info!("starting dev Kubernetes cluster (k3s)");
    let mut compose = k8s_compose(workspace, sock_dir)?;
    compose
        .up()
        .await
        .context("start k8s fixture compose stack")?;
    tracing::info!("dev Kubernetes cluster ready");
    Ok(compose)
}

async fn teardown_k8s_compose(workspace: &Path, sock_dir: &Path) -> Result<()> {
    let compose = k8s_compose(workspace, sock_dir)?;
    compose
        .down()
        .await
        .context("stop orphaned k8s fixture compose stack")?;
    Ok(())
}

async fn remove_runtime_container(container_name: &str) -> Result<()> {
    let docker = Docker::connect_with_local_defaults()
        .context("connect to Docker daemon for runtime container teardown")?;
    match docker
        .remove_container(
            container_name,
            Some(RemoveContainerOptions {
                force: true,
                ..Default::default()
            }),
        )
        .await
    {
        Ok(()) => Ok(()),
        Err(error) => {
            tracing::debug!("runtime container `{container_name}` removal: {error:#}");
            Ok(())
        },
    }
}

async fn remove_db_container(container_id: &str) -> Result<()> {
    let docker = Docker::connect_with_local_defaults()
        .context("connect to Docker daemon for fixture teardown")?;
    match docker
        .remove_container(
            container_id,
            Some(RemoveContainerOptions {
                force: true,
                ..Default::default()
            }),
        )
        .await
    {
        Ok(()) => Ok(()),
        Err(error) => {
            tracing::debug!("db fixture container `{container_id}` removal: {error:#}");
            Ok(())
        },
    }
}

async fn teardown_fixtures(record: &DevSessionRecord) -> Result<()> {
    if record.fixtures.k8s {
        let sock_dir = record
            .fixtures
            .k8s_sock_dir
            .as_deref()
            .unwrap_or_else(|| Path::new("/tmp/omnifs-k8s-sock"));
        if let Err(error) = teardown_k8s_compose(&record.workspace, sock_dir).await {
            tracing::debug!("k8s fixture teardown: {error:#}");
        }
    }

    if let Some(container_id) = &record.fixtures.db_container_id
        && let Err(error) = remove_db_container(container_id).await
    {
        tracing::debug!("db fixture container `{container_id}` teardown: {error:#}");
    }

    Ok(())
}

/// Tear down fixtures recorded in a dev session file. Best-effort and idempotent.
pub async fn teardown_from_session(session_path: &Path) -> Result<()> {
    let dev_home = session_path
        .parent()
        .context("dev session path must have a parent directory")?;
    DevSessionRecord::teardown_all(dev_home).await
}

/// Minimal dev session state for crash recovery and `omnifs down` orphan sweep.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct DevSessionRecord {
    pub workspace: PathBuf,
    pub profile: String,
    pub container_name: String,
    pub fixtures: DevSessionFixtures,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct DevSessionFixtures {
    pub k8s: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub k8s_sock_dir: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub db_container_id: Option<String>,
}

impl DevSessionRecord {
    pub fn read(dev_home: &Path) -> Result<Option<Self>> {
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

    pub fn write(&self, dev_home: &Path) -> Result<()> {
        let path = dev_home.join("session.json");
        let json = serde_json::to_string_pretty(self).context("serialize dev session")?;
        std::fs::write(&path, format!("{json}\n"))
            .with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }

    pub fn clear(dev_home: &Path) -> Result<()> {
        let path = dev_home.join("session.json");
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
        }
    }

    pub fn clear_launch_record(dev_home: &Path) -> Result<()> {
        let path = dev_home.join("launch.json");
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
        }
    }

    /// Full dev session teardown: runtime container, fixtures, session and launch records.
    pub async fn teardown_all(dev_home: &Path) -> Result<()> {
        let Some(record) = Self::read(dev_home)? else {
            return Ok(());
        };

        if let Err(error) = remove_runtime_container(&record.container_name).await {
            tracing::debug!("runtime container teardown: {error:#}");
        }

        teardown_fixtures(&record).await?;

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

    #[tokio::test]
    async fn teardown_all_without_session_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        DevSessionRecord::teardown_all(dir.path()).await.unwrap();
        DevSessionRecord::teardown_all(dir.path()).await.unwrap();
    }

    #[tokio::test]
    async fn teardown_all_clears_session_files() {
        let dir = tempfile::tempdir().unwrap();
        let record = DevSessionRecord {
            workspace: dir.path().join("workspace"),
            profile: "smoke".to_string(),
            container_name: "omnifs-dev-missing".to_string(),
            fixtures: DevSessionFixtures::default(),
        };
        record.write(dir.path()).unwrap();
        std::fs::write(dir.path().join("launch.json"), "{}\n").unwrap();

        DevSessionRecord::teardown_all(dir.path()).await.unwrap();

        assert!(!dir.path().join("session.json").exists());
        assert!(!dir.path().join("launch.json").exists());
    }
}
