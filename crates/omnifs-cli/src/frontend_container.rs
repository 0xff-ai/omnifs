//! The Docker-hosted FUSE frontend container: naming, image resolution, and
//! the container body it launches with.
//!
//! Kept apart from `runtime.rs` because the frontend container is a separate
//! delivery mechanism: no home bind mount, no credentials, no control socket
//! exposure. It attaches to a host-native daemon's TCP namespace listener
//! instead of running the daemon itself. See `docs/contracts/50-control-plane.md`.

use std::collections::HashMap;
use std::path::Path;

use bollard::models::{ContainerCreateBody, DeviceMapping, HostConfig, MountPoint};
use omnifs_api::{OMNIFS_ATTACH_ADDR_ENV, OMNIFS_ATTACH_TOKEN_ENV};
use omnifs_workspace::OMNIFS_HOME_ENV;

use crate::docker::ContainerName;
use crate::image::{BUILD_CHANNEL, BuildChannel, ImageRef};
use omnifs_workspace::config::Config;

/// Base container name for the default workspace. A non-default workspace
/// (an explicit `OMNIFS_HOME`) disambiguates with an 8-hex-char content hash
/// of its config dir, so more than one workspace can run a frontend container
/// at once without colliding.
pub(crate) const FRONTEND_CONTAINER_BASE: &str = "omnifs-frontend";

pub(crate) const FRONTEND_RELEASE_IMAGE: &str = concat!(
    "ghcr.io/0xff-ai/omnifs-frontend:",
    env!("CARGO_PKG_VERSION")
);
pub(crate) const FRONTEND_DEV_IMAGE: &str = "omnifs-frontend:dev";
pub(crate) const ENV_FRONTEND_IMAGE: &str = "OMNIFS_FRONTEND_IMAGE";

/// Label recording the workspace a frontend container belongs to, for
/// `docker ps --filter` discovery and the fail-closed lockdown check.
pub(crate) const FRONTEND_HOME_LABEL: &str = "ai.0xff.omnifs.home";

pub(crate) const fn default_frontend_image_for(channel: BuildChannel) -> &'static str {
    match channel {
        BuildChannel::Release => FRONTEND_RELEASE_IMAGE,
        BuildChannel::Dev => FRONTEND_DEV_IMAGE,
    }
}

/// Resolve the frontend image through the flag > env > config > default
/// precedence chain (CLI flag, environment, workspace config, then default), gated on the
/// build channel: a release binary defaults to the pinned registry tag, a dev
/// binary defaults to the local `omnifs-frontend:dev` tag and never pulls.
pub(crate) fn resolve_frontend_image(
    image: Option<String>,
    config: &Config,
) -> anyhow::Result<ImageRef> {
    let image = image
        .or_else(|| std::env::var(ENV_FRONTEND_IMAGE).ok())
        .or_else(|| config.system.frontend_image.clone())
        .unwrap_or_else(|| default_frontend_image_for(BUILD_CHANNEL).to_string());
    ImageRef::new(image)
}

/// The frontend container's name: the bare base name for the default
/// workspace (no `OMNIFS_HOME` override), else the base name suffixed with an
/// 8-hex-char hash of the config dir so multiple workspaces never collide.
pub(crate) fn frontend_container_name(config_dir: &Path) -> anyhow::Result<ContainerName> {
    container_name_for(config_dir, std::env::var_os(OMNIFS_HOME_ENV).is_none())
}

fn container_name_for(config_dir: &Path, is_default_home: bool) -> anyhow::Result<ContainerName> {
    let name = if is_default_home {
        FRONTEND_CONTAINER_BASE.to_string()
    } else {
        format!("{FRONTEND_CONTAINER_BASE}-{}", hash8(config_dir))
    };
    ContainerName::new(name)
}

/// An 8-hex-char (32-bit) content hash of `path`, collision-resistant enough
/// to disambiguate a handful of concurrent dev/test workspaces on one host.
fn hash8(path: &Path) -> String {
    let digest = blake3::hash(path.to_string_lossy().as_bytes());
    hex::encode(&digest.as_bytes()[..4])
}

/// Everything [`FrontendContainerSpec::build_body`] needs, gathered so the
/// no-credentials contract (see `docs/contracts/50-control-plane.md`) is
/// visible at one call site: no binds, no `OMNIFS_HOME`, no docker.sock, no
/// SSH agent, no published ports.
pub(crate) struct FrontendContainerSpec<'a> {
    pub image: &'a ImageRef,
    /// The workspace's config dir, recorded as a label only (never bind-mounted).
    pub home: &'a Path,
    /// The host-native daemon's TCP namespace attach port. The container
    /// dials it at `host.docker.internal:<port>`, the Docker-injected DNS
    /// name for the host, not a literal address the CLI could resolve ahead
    /// of time.
    pub attach_port: u16,
    pub attach_token: &'a str,
    /// `--add-host host.docker.internal:host-gateway`: required on Linux,
    /// where Docker does not predefine the name; Docker Desktop (macOS)
    /// already resolves it without this flag.
    pub add_host_gateway: bool,
}

impl FrontendContainerSpec<'_> {
    pub(crate) fn build_body(&self) -> ContainerCreateBody {
        let mut labels = HashMap::new();
        labels.insert(
            FRONTEND_HOME_LABEL.to_string(),
            self.home.display().to_string(),
        );

        let extra_hosts = self
            .add_host_gateway
            .then(|| vec!["host.docker.internal:host-gateway".to_string()]);

        let host_config = HostConfig {
            devices: Some(vec![DeviceMapping {
                path_on_host: Some("/dev/fuse".to_string()),
                path_in_container: Some("/dev/fuse".to_string()),
                cgroup_permissions: Some("rwm".to_string()),
            }]),
            cap_add: Some(vec!["SYS_ADMIN".to_string()]),
            security_opt: Some(vec!["apparmor:unconfined".to_string()]),
            extra_hosts,
            ..Default::default()
        };

        let env = vec![
            format!(
                "{OMNIFS_ATTACH_ADDR_ENV}=host.docker.internal:{}",
                self.attach_port
            ),
            format!("{OMNIFS_ATTACH_TOKEN_ENV}={}", self.attach_token),
        ];

        ContainerCreateBody {
            image: Some(self.image.as_str().to_string()),
            env: Some(env),
            labels: Some(labels),
            host_config: Some(host_config),
            ..Default::default()
        }
    }
}

/// Env var names the frontend container's image may set on its own (its
/// `Dockerfile` `ENV`/base-image defaults), beyond the two attach vars this
/// launcher injects. Anything else on a freshly started container means
/// something leaked onto this credential-free container.
const IMAGE_DEFAULT_ENV_NAMES: [&str; 2] = ["PATH", "HOME"];

/// Fail-closed structural assertion, run immediately after `docker inspect`
/// on a just-started frontend container: no mounts of any kind, and an env
/// set that is exactly the two attach vars plus the image's own defaults.
/// Returns the violation message on failure; the caller kills the container.
pub(crate) fn assert_locked_down(mounts: &[MountPoint], env: &[String]) -> Result<(), String> {
    if !mounts.is_empty() {
        return Err(format!(
            "frontend container has {}; the no-credentials contract allows none",
            crate::ui::render::count(mounts.len(), "mount")
        ));
    }
    if let Some(bad) = env.iter().find(|var| !env_var_allowed(var)) {
        return Err(format!(
            "frontend container has unexpected env var `{bad}`; the no-credentials contract \
             allows only {OMNIFS_ATTACH_ADDR_ENV}, {OMNIFS_ATTACH_TOKEN_ENV}, and the image's own defaults"
        ));
    }
    Ok(())
}

fn env_var_allowed(var: &str) -> bool {
    let Some((name, _)) = var.split_once('=') else {
        return false;
    };
    name == OMNIFS_ATTACH_ADDR_ENV
        || name == OMNIFS_ATTACH_TOKEN_ENV
        || IMAGE_DEFAULT_ENV_NAMES.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_workspace::config::System;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[allow(unsafe_code)] // env::set_var/remove_var require unsafe; guarded by ENV_LOCK.
    fn with_env<F: FnOnce()>(vars: &[(&str, Option<&str>)], f: F) {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let saved: Vec<(&str, Option<String>)> = vars
            .iter()
            .map(|(key, _)| (*key, std::env::var(*key).ok()))
            .collect();
        // SAFETY: ENV_LOCK is held for the entire duration of this call.
        for (key, value) in vars {
            match value {
                Some(v) => unsafe { std::env::set_var(key, v) },
                None => unsafe { std::env::remove_var(key) },
            }
        }
        f();
        // SAFETY: ENV_LOCK is still held.
        for (key, original) in &saved {
            match original {
                Some(v) => unsafe { std::env::set_var(key, v) },
                None => unsafe { std::env::remove_var(key) },
            }
        }
    }

    #[test]
    fn dev_channel_defaults_to_local_frontend_dev_image() {
        assert_eq!(
            default_frontend_image_for(BuildChannel::Dev),
            "omnifs-frontend:dev"
        );
    }

    #[test]
    fn release_channel_defaults_to_pinned_frontend_registry_tag() {
        assert!(
            default_frontend_image_for(BuildChannel::Release)
                .starts_with("ghcr.io/0xff-ai/omnifs-frontend:")
        );
    }

    #[test]
    fn frontend_image_resolution_precedence() {
        with_env(&[(ENV_FRONTEND_IMAGE, None)], || {
            let config = Config {
                system: System {
                    frontend_image: Some("ghcr.io/example/frontend-config:1.0.0".into()),
                },
                ..Default::default()
            };
            let image = resolve_frontend_image(None, &config).unwrap();
            assert_eq!(image.as_str(), "ghcr.io/example/frontend-config:1.0.0");

            let image =
                resolve_frontend_image(Some("ghcr.io/example/frontend-flag:2.0.0".into()), &config)
                    .unwrap();
            assert_eq!(image.as_str(), "ghcr.io/example/frontend-flag:2.0.0");
        });

        with_env(
            &[(
                ENV_FRONTEND_IMAGE,
                Some("ghcr.io/example/frontend-env:9.9.9"),
            )],
            || {
                let config = Config::default();
                let image = resolve_frontend_image(None, &config).unwrap();
                assert_eq!(image.as_str(), "ghcr.io/example/frontend-env:9.9.9");
            },
        );
    }

    #[test]
    fn default_home_uses_bare_container_name() {
        let name = container_name_for(Path::new("/home/u/.omnifs"), true).unwrap();
        assert_eq!(name.as_str(), FRONTEND_CONTAINER_BASE);
    }

    #[test]
    fn non_default_home_gets_a_stable_hashed_suffix() {
        let name_a = container_name_for(Path::new("/home/u/.omnifs-dev"), false).unwrap();
        let name_b = container_name_for(Path::new("/home/u/.omnifs-dev"), false).unwrap();
        let name_other = container_name_for(Path::new("/home/u/.omnifs-other"), false).unwrap();

        assert_eq!(name_a, name_b, "the same home must hash to the same name");
        assert_ne!(
            name_a, name_other,
            "different homes must not collide on one container name"
        );
        assert!(name_a.as_str().starts_with(FRONTEND_CONTAINER_BASE));
        assert_ne!(name_a.as_str(), FRONTEND_CONTAINER_BASE);
    }

    #[test]
    fn container_body_carries_no_binds_and_the_two_attach_vars() {
        let image = ImageRef::new("omnifs-frontend:dev").unwrap();
        let spec = FrontendContainerSpec {
            image: &image,
            home: Path::new("/home/u/.omnifs"),
            attach_port: 54321,
            attach_token: "test-token",
            add_host_gateway: true,
        };
        let body = spec.build_body();

        assert_eq!(body.image.as_deref(), Some("omnifs-frontend:dev"));

        let host_config = body.host_config.expect("host config");
        assert!(
            host_config.binds.is_none() || host_config.binds == Some(Vec::new()),
            "the frontend container must carry no binds: {:?}",
            host_config.binds
        );
        assert_eq!(
            host_config.devices.as_deref().map(<[_]>::len),
            Some(1),
            "expected exactly the /dev/fuse device mapping"
        );
        assert_eq!(
            host_config.extra_hosts,
            Some(vec!["host.docker.internal:host-gateway".to_string()])
        );

        let env = body.env.expect("env");
        assert_eq!(
            env.len(),
            2,
            "expected exactly the two attach vars: {env:?}"
        );
        assert!(
            env.iter()
                .any(|e| e == &format!("{OMNIFS_ATTACH_ADDR_ENV}=host.docker.internal:54321"))
        );
        assert!(
            env.iter()
                .any(|e| e == &format!("{OMNIFS_ATTACH_TOKEN_ENV}=test-token"))
        );

        let labels = body.labels.expect("labels");
        assert_eq!(
            labels.get(FRONTEND_HOME_LABEL).map(String::as_str),
            Some("/home/u/.omnifs")
        );
    }

    #[test]
    fn macos_omits_add_host_gateway() {
        let image = ImageRef::new("omnifs-frontend:dev").unwrap();
        let spec = FrontendContainerSpec {
            image: &image,
            home: Path::new("/home/u/.omnifs"),
            attach_port: 1,
            attach_token: "t",
            add_host_gateway: false,
        };
        let body = spec.build_body();
        assert_eq!(body.host_config.unwrap().extra_hosts, None);
    }

    #[test]
    fn lockdown_rejects_any_mount() {
        let err = assert_locked_down(&[MountPoint::default()], &[]).unwrap_err();
        assert!(err.contains("mount"));
    }

    #[test]
    fn lockdown_allows_only_attach_vars_and_image_defaults() {
        assert_locked_down(
            &[],
            &[
                "PATH=/usr/bin".to_string(),
                "HOME=/root".to_string(),
                format!("{OMNIFS_ATTACH_ADDR_ENV}=host.docker.internal:1"),
                format!("{OMNIFS_ATTACH_TOKEN_ENV}=abc"),
            ],
        )
        .expect("the exact allowed set must pass");
    }

    #[test]
    fn lockdown_rejects_an_unexpected_env_var() {
        let err = assert_locked_down(&[], &["OMNIFS_HOME=/root/.omnifs".to_string()]).unwrap_err();
        assert!(err.contains("OMNIFS_HOME"));
    }
}
