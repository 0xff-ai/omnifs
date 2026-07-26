//! The Docker-hosted FUSE filesystem container: naming, image resolution, and
//! the container body it launches with.
//!
//! Kept apart from `runtime.rs` because the filesystem container is a separate
//! delivery mechanism: no home bind mount, no credentials, no control socket
//! exposure. It attaches to a host-native daemon's TCP namespace listener
//! instead of running the daemon itself. See `docs/contracts/50-control-plane.md`.

use std::collections::HashMap;
use std::path::Path;

use bollard::models::{ContainerCreateBody, DeviceMapping, HostConfig, MountPoint};
use omnifs_api::OMNIFS_ATTACH_ADDR_ENV;
use omnifs_core::fs;
use omnifs_workspace::OMNIFS_HOME_ENV;

use crate::docker::ContainerName;
use crate::image::{BUILD_CHANNEL, BuildChannel, ImageRef};
use omnifs_workspace::config::Config;

/// Base container name for the default workspace. A non-default workspace
/// (an explicit `OMNIFS_HOME`) disambiguates with an 8-hex-char content hash
/// of its config dir, so more than one workspace can run a filesystem container
/// at once without colliding.
pub(crate) const FILESYSTEM_CONTAINER_BASE: &str = "omnifs-fs";

pub(crate) const FILESYSTEM_RELEASE_IMAGE: &str = concat!(
    "ghcr.io/0xff-ai/omnifs-filesystem:",
    env!("CARGO_PKG_VERSION")
);
pub(crate) const FILESYSTEM_DEV_IMAGE: &str = "omnifs-filesystem:dev";
pub(crate) const ENV_FILESYSTEM_IMAGE: &str = "OMNIFS_FILESYSTEM_IMAGE";

/// Label recording the workspace a filesystem container belongs to, for
/// `docker ps --filter` discovery and the fail-closed lockdown check.
pub(crate) const FILESYSTEM_HOME_LABEL: &str = "ai.0xff.omnifs.home";
pub(crate) const FILESYSTEM_ID_LABEL: &str = "ai.0xff.omnifs.fs";

pub(crate) const fn default_filesystem_image_for(channel: BuildChannel) -> &'static str {
    match channel {
        BuildChannel::Release => FILESYSTEM_RELEASE_IMAGE,
        BuildChannel::Dev => FILESYSTEM_DEV_IMAGE,
    }
}

/// Resolve the filesystem image through the flag > env > config > default
/// precedence chain (CLI flag, environment, workspace config, then default), gated on the
/// build channel: a release binary defaults to the pinned registry tag, a dev
/// binary defaults to the local `omnifs-filesystem:dev` tag and never pulls.
pub(crate) fn resolve_filesystem_image(
    image: Option<String>,
    config: &Config,
) -> anyhow::Result<ImageRef> {
    let image = image
        .or_else(|| std::env::var(ENV_FILESYSTEM_IMAGE).ok())
        .or_else(|| config.filesystem.docker_image.clone())
        .unwrap_or_else(|| default_filesystem_image_for(BUILD_CHANNEL).to_string());
    ImageRef::new(image)
}

/// The filesystem container's name: the bare base name for the default
/// workspace (no `OMNIFS_HOME` override), else the base name suffixed with an
/// 8-hex-char hash of the config dir so multiple workspaces never collide.
pub(crate) fn filesystem_container_name(
    config_dir: &Path,
    id: &fs::Id,
) -> anyhow::Result<ContainerName> {
    container_name_for(config_dir, id, std::env::var_os(OMNIFS_HOME_ENV).is_none())
}

fn container_name_for(
    config_dir: &Path,
    id: &fs::Id,
    is_default_home: bool,
) -> anyhow::Result<ContainerName> {
    let name = if is_default_home {
        format!("{FILESYSTEM_CONTAINER_BASE}-{id}")
    } else {
        format!("{FILESYSTEM_CONTAINER_BASE}-{}-{id}", hash8(config_dir))
    };
    ContainerName::new(name)
}

/// An 8-hex-char (32-bit) content hash of `path`, collision-resistant enough
/// to disambiguate a handful of concurrent dev/test workspaces on one host.
fn hash8(path: &Path) -> String {
    let digest = blake3::hash(path.to_string_lossy().as_bytes());
    hex::encode(&digest.as_bytes()[..4])
}

/// Build the credential-free filesystem container body: no binds, `OMNIFS_HOME`,
/// Docker socket, SSH agent, or published ports. Only the attach address is
/// injected as env; the resolved filesystem spec is passed as flat argv.
pub(crate) fn build_filesystem_container_body(
    image: &ImageRef,
    home: &Path,
    spec: &fs::Spec,
    attach_port: u16,
    add_host_gateway: bool,
) -> ContainerCreateBody {
    let mut labels = HashMap::new();
    labels.insert(
        FILESYSTEM_HOME_LABEL.to_string(),
        home.display().to_string(),
    );
    labels.insert(FILESYSTEM_ID_LABEL.to_string(), spec.id().to_string());

    let extra_hosts =
        add_host_gateway.then(|| vec!["host.docker.internal:host-gateway".to_string()]);

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

    let env = vec![format!(
        "{OMNIFS_ATTACH_ADDR_ENV}=host.docker.internal:{attach_port}"
    )];
    let cmd = filesystem_command(spec);

    ContainerCreateBody {
        image: Some(image.as_str().to_string()),
        env: Some(env),
        cmd: Some(cmd),
        labels: Some(labels),
        host_config: Some(host_config),
        ..Default::default()
    }
}

pub(crate) fn filesystem_command(spec: &fs::Spec) -> Vec<String> {
    vec![
        "--name".to_owned(),
        spec.id().to_string(),
        "--protocol".to_owned(),
        spec.protocol().to_string(),
        "--runtime".to_owned(),
        spec.runtime().to_string(),
        "--location".to_owned(),
        spec.location().display().to_string(),
    ]
}

/// Env var names the filesystem container's image may set on its own (its
/// `Dockerfile` `ENV`/base-image defaults), beyond the two values this
/// launcher injects. Anything else on a freshly started container means
/// something leaked onto this credential-free container.
const IMAGE_DEFAULT_ENV_NAMES: [&str; 2] = ["PATH", "HOME"];

/// Fail-closed structural assertion, run immediately after `docker inspect`
/// on a just-started filesystem container: no mounts of any kind, and an env
/// set that is exactly the attach addr plus the image's own defaults.
/// Returns the violation message on failure; the caller kills the container.
pub(crate) fn assert_locked_down(mounts: &[MountPoint], env: &[String]) -> Result<(), String> {
    if !mounts.is_empty() {
        return Err(format!(
            "filesystem container has {}; the no-credentials contract allows none",
            crate::ui::render::count(mounts.len(), "mount")
        ));
    }
    let mut names = std::collections::HashSet::new();
    for var in env {
        if !env_var_allowed(var) {
            return Err(format!(
                "filesystem container has unexpected env var `{var}`; the no-credentials contract \
                 allows only {OMNIFS_ATTACH_ADDR_ENV} and the image's own defaults"
            ));
        }
        let name = var
            .split_once('=')
            .map(|(name, _)| name)
            .expect("env_var_allowed requires KEY=VALUE");
        if !names.insert(name) {
            return Err(format!(
                "filesystem container has duplicate env var `{name}`"
            ));
        }
    }
    if !names.contains(OMNIFS_ATTACH_ADDR_ENV) {
        return Err(format!(
            "filesystem container is missing required env var `{OMNIFS_ATTACH_ADDR_ENV}`"
        ));
    }
    Ok(())
}

fn env_var_allowed(var: &str) -> bool {
    let Some((name, _)) = var.split_once('=') else {
        return false;
    };
    name == OMNIFS_ATTACH_ADDR_ENV || IMAGE_DEFAULT_ENV_NAMES.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_workspace::config::FilesystemAssets;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn spec() -> fs::Spec {
        fs::Spec::new(
            "work".parse().unwrap(),
            fs::Protocol::Fuse,
            fs::Runtime::Docker,
            fs::GUEST_LOCATION.into(),
        )
        .unwrap()
    }

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
    fn dev_channel_defaults_to_local_filesystem_dev_image() {
        assert_eq!(
            default_filesystem_image_for(BuildChannel::Dev),
            "omnifs-filesystem:dev"
        );
    }

    #[test]
    fn release_channel_defaults_to_pinned_filesystem_registry_tag() {
        assert!(
            default_filesystem_image_for(BuildChannel::Release)
                .starts_with("ghcr.io/0xff-ai/omnifs-filesystem:")
        );
    }

    #[test]
    fn filesystem_image_resolution_precedence() {
        with_env(&[(ENV_FILESYSTEM_IMAGE, None)], || {
            let config = Config {
                filesystem: FilesystemAssets {
                    docker_image: Some("ghcr.io/example/filesystem-config:1.0.0".into()),
                    ..Default::default()
                },
                ..Default::default()
            };
            let image = resolve_filesystem_image(None, &config).unwrap();
            assert_eq!(image.as_str(), "ghcr.io/example/filesystem-config:1.0.0");

            let image = resolve_filesystem_image(
                Some("ghcr.io/example/filesystem-flag:2.0.0".into()),
                &config,
            )
            .unwrap();
            assert_eq!(image.as_str(), "ghcr.io/example/filesystem-flag:2.0.0");
        });

        with_env(
            &[(
                ENV_FILESYSTEM_IMAGE,
                Some("ghcr.io/example/filesystem-env:9.9.9"),
            )],
            || {
                let config = Config::default();
                let image = resolve_filesystem_image(None, &config).unwrap();
                assert_eq!(image.as_str(), "ghcr.io/example/filesystem-env:9.9.9");
            },
        );
    }

    #[test]
    fn default_home_uses_bare_container_name() {
        let id = "work".parse().unwrap();
        let name = container_name_for(Path::new("/home/u/.omnifs"), &id, true).unwrap();
        assert_eq!(name.as_str(), "omnifs-fs-work");
    }

    #[test]
    fn non_default_home_gets_a_stable_hashed_suffix() {
        let id = "work".parse().unwrap();
        let name_a = container_name_for(Path::new("/home/u/.omnifs-dev"), &id, false).unwrap();
        let name_b = container_name_for(Path::new("/home/u/.omnifs-dev"), &id, false).unwrap();
        let name_other =
            container_name_for(Path::new("/home/u/.omnifs-other"), &id, false).unwrap();

        assert_eq!(name_a, name_b, "the same home must hash to the same name");
        assert_ne!(
            name_a, name_other,
            "different homes must not collide on one container name"
        );
        assert!(name_a.as_str().starts_with(FILESYSTEM_CONTAINER_BASE));
    }

    #[test]
    fn container_body_carries_no_binds_and_the_attach_address() {
        let image = ImageRef::new("omnifs-filesystem:dev").unwrap();
        let body = build_filesystem_container_body(
            &image,
            Path::new("/home/u/.omnifs"),
            &spec(),
            54321,
            true,
        );

        assert_eq!(body.image.as_deref(), Some("omnifs-filesystem:dev"));

        let host_config = body.host_config.expect("host config");
        assert!(
            host_config.binds.is_none() || host_config.binds == Some(Vec::new()),
            "the filesystem container must carry no binds: {:?}",
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
            env,
            vec![format!(
                "{OMNIFS_ATTACH_ADDR_ENV}=host.docker.internal:54321"
            )]
        );

        let labels = body.labels.expect("labels");
        assert_eq!(
            labels.get(FILESYSTEM_HOME_LABEL).map(String::as_str),
            Some("/home/u/.omnifs")
        );
        assert_eq!(
            labels.get(FILESYSTEM_ID_LABEL).map(String::as_str),
            Some("work")
        );
        assert_eq!(body.cmd, Some(filesystem_command(&spec())));
    }

    #[test]
    fn macos_omits_add_host_gateway() {
        let image = ImageRef::new("omnifs-filesystem:dev").unwrap();
        let body = build_filesystem_container_body(
            &image,
            Path::new("/home/u/.omnifs"),
            &spec(),
            1,
            false,
        );
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
            ],
        )
        .expect("the exact allowed set must pass");
    }

    #[test]
    fn lockdown_rejects_an_unexpected_env_var() {
        let err = assert_locked_down(&[], &["OMNIFS_HOME=/root/.omnifs".to_string()]).unwrap_err();
        assert!(err.contains("OMNIFS_HOME"));
    }

    #[test]
    fn lockdown_requires_one_attach_address() {
        let missing = assert_locked_down(&[], &["PATH=/usr/bin".to_string()]).unwrap_err();
        assert!(missing.contains(OMNIFS_ATTACH_ADDR_ENV));
        let duplicate = assert_locked_down(
            &[],
            &[
                format!("{OMNIFS_ATTACH_ADDR_ENV}=host.docker.internal:1"),
                format!("{OMNIFS_ATTACH_ADDR_ENV}=host.docker.internal:2"),
            ],
        )
        .unwrap_err();
        assert!(duplicate.contains("duplicate"));
    }
}
