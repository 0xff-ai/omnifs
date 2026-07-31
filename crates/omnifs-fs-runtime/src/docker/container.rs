//! The Docker-hosted FUSE filesystem container: naming, image resolution, and
//! the container body it launches with. A submodule of `docker` (not a
//! top-level module) because this is Docker-private knowledge: every type it
//! touches (`ContainerCreateBody`, `HostConfig`, `DeviceMapping`, `MountPoint`)
//! is bollard-specific, and its naming/body-building belongs on `DockerTarget`.
//!
//! Still a separate delivery mechanism from the daemon itself: no home bind
//! mount, no credentials, no control socket exposure. It attaches to a
//! host-native daemon's TCP namespace listener instead of running the daemon
//! itself. See `docs/contracts/50-control-plane.md`.

use std::collections::HashMap;
use std::path::Path;

use bollard::models::{ContainerCreateBody, DeviceMapping, HostConfig, MountPoint};
use omnifs_api::OMNIFS_ATTACH_ADDR_ENV;
#[cfg(test)]
use omnifs_core::{FILESYSTEM_GUEST_LOCATION, FilesystemProtocol, FilesystemRuntime};
use omnifs_core::{FilesystemSpec, ResourceName};

use super::{ContainerName, DockerTarget};
use crate::{BUILD_CHANNEL, BuildChannel, ImageRef};

/// Base container name for the default profile. A non-default profile
/// (an explicit `OMNIFS_HOME`) disambiguates with an 8-hex-char content hash
/// of its profile root, so more than one profile can run a filesystem container
/// at once without colliding.
pub(crate) const FILESYSTEM_CONTAINER_BASE: &str = "omnifs-fs";

pub(crate) const FILESYSTEM_RELEASE_IMAGE: &str = concat!(
    "ghcr.io/0xff-ai/omnifs-filesystem:",
    env!("CARGO_PKG_VERSION")
);
pub(crate) const FILESYSTEM_DEV_IMAGE: &str = "omnifs-filesystem:dev";
pub(crate) const ENV_FILESYSTEM_IMAGE: &str = "OMNIFS_FILESYSTEM_IMAGE";

/// Label recording the profile a filesystem container belongs to, for
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
/// precedence chain (explicit value, environment, profile config, then
/// default), gated on the build channel: a release binary defaults to the
/// pinned registry tag, while a dev binary defaults to the local
/// `omnifs-filesystem:dev` tag and never pulls.
pub fn resolve_filesystem_image(
    image: Option<String>,
    configured: Option<&str>,
) -> anyhow::Result<ImageRef> {
    let image = image
        .or_else(|| std::env::var(ENV_FILESYSTEM_IMAGE).ok())
        .or_else(|| configured.map(str::to_owned))
        .unwrap_or_else(|| default_filesystem_image_for(BUILD_CHANNEL).to_string());
    ImageRef::new(image)
}

impl DockerTarget {
    /// The filesystem container's name: the bare base name for the default
    /// profile (no `OMNIFS_HOME` override), else the base name suffixed with
    /// an 8-hex-char hash of the profile root so multiple profiles never
    /// collide.
    pub(crate) fn filesystem_container_name(
        config_dir: &Path,
        filesystem: &ResourceName,
        is_default_home: bool,
    ) -> anyhow::Result<ContainerName> {
        container_name_for(config_dir, filesystem, is_default_home)
    }
}

fn container_name_for(
    config_dir: &Path,
    filesystem: &ResourceName,
    is_default_home: bool,
) -> anyhow::Result<ContainerName> {
    let name = if is_default_home {
        format!("{FILESYSTEM_CONTAINER_BASE}-{filesystem}")
    } else {
        format!(
            "{FILESYSTEM_CONTAINER_BASE}-{}-{filesystem}",
            hash8(config_dir)
        )
    };
    ContainerName::new(name)
}

/// An 8-hex-char (32-bit) content hash of `path`, collision-resistant enough
/// to disambiguate a handful of concurrent dev/test workspaces on one host.
fn hash8(path: &Path) -> String {
    use sha2::{Digest as _, Sha256};
    let digest = Sha256::digest(path.to_string_lossy().as_bytes());
    hex::encode(&digest[..4])
}

impl DockerTarget {
    /// Build the credential-free filesystem container body: no binds,
    /// `OMNIFS_HOME`, Docker socket, SSH agent, or published ports. Only the
    /// attach address is injected as env; the resolved Filesystem spec is
    /// passed as flat argv.
    pub(crate) fn build_filesystem_container_body(
        &self,
        home: &Path,
        filesystem: &ResourceName,
        spec: &FilesystemSpec,
        runtime_instance: &str,
        attach_port: u16,
        add_host_gateway: bool,
    ) -> ContainerCreateBody {
        let mut labels = HashMap::new();
        labels.insert(
            FILESYSTEM_HOME_LABEL.to_string(),
            home.display().to_string(),
        );
        labels.insert(FILESYSTEM_ID_LABEL.to_string(), filesystem.to_string());

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
        let cmd = filesystem_command(filesystem, spec, runtime_instance);

        ContainerCreateBody {
            image: Some(self.image().as_str().to_string()),
            env: Some(env),
            cmd: Some(cmd),
            labels: Some(labels),
            host_config: Some(host_config),
            ..Default::default()
        }
    }
}

pub(crate) fn filesystem_command(
    filesystem: &ResourceName,
    spec: &FilesystemSpec,
    runtime_instance: &str,
) -> Vec<String> {
    let mut command = vec![
        "--name".to_owned(),
        filesystem.to_string(),
        "--protocol".to_owned(),
        spec.protocol().to_string(),
        "--runtime".to_owned(),
        spec.runtime().to_string(),
        "--location".to_owned(),
        spec.location().display().to_string(),
    ];
    if let Some(image) = spec.docker_image() {
        command.extend(["--docker-image".to_owned(), image.to_owned()]);
    }
    if let Some(image) = spec.libkrun_guest_image() {
        command.extend(["--libkrun-guest-image".to_owned(), image.to_owned()]);
    }
    command.extend(["--runtime-instance".to_owned(), runtime_instance.to_owned()]);
    command
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
            count(mounts.len(), "mount")
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

fn count(value: usize, noun: &str) -> String {
    if value == 1 {
        format!("1 {noun}")
    } else {
        format!("{value} {noun}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filesystem() -> ResourceName {
        "work".parse().unwrap()
    }
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn spec() -> FilesystemSpec {
        FilesystemSpec::new(
            FilesystemProtocol::Fuse,
            FilesystemRuntime::Docker,
            FILESYSTEM_GUEST_LOCATION.into(),
            Some("omnifs-filesystem:dev".into()),
            None,
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
            let configured = Some("ghcr.io/example/filesystem-config:1.0.0");
            let image = resolve_filesystem_image(None, configured).unwrap();
            assert_eq!(image.as_str(), "ghcr.io/example/filesystem-config:1.0.0");

            let image = resolve_filesystem_image(
                Some("ghcr.io/example/filesystem-flag:2.0.0".into()),
                configured,
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
                let image = resolve_filesystem_image(None, None).unwrap();
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

    fn target(image: &str) -> DockerTarget {
        DockerTarget::new("omnifs-fs-test".to_owned(), image.to_owned()).unwrap()
    }

    #[test]
    fn container_body_carries_no_binds_and_the_attach_address() {
        let body = target("omnifs-filesystem:dev").build_filesystem_container_body(
            Path::new("/home/u/.omnifs"),
            &filesystem(),
            &spec(),
            "runtime",
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
        assert_eq!(
            body.cmd,
            Some(filesystem_command(&filesystem(), &spec(), "runtime"))
        );
    }

    #[test]
    fn container_command_preserves_exact_docker_image() {
        let spec = FilesystemSpec::new(
            FilesystemProtocol::Fuse,
            FilesystemRuntime::Docker,
            FILESYSTEM_GUEST_LOCATION.into(),
            Some("omnifs-filesystem:exact".to_owned()),
            None,
        )
        .unwrap();

        assert_eq!(
            filesystem_command(&filesystem(), &spec, "runtime"),
            vec![
                "--name",
                "work",
                "--protocol",
                "fuse",
                "--runtime",
                "docker",
                "--location",
                "/omnifs",
                "--docker-image",
                "omnifs-filesystem:exact",
                "--runtime-instance",
                "runtime",
            ]
        );
    }

    #[test]
    fn macos_omits_add_host_gateway() {
        let body = target("omnifs-filesystem:dev").build_filesystem_container_body(
            Path::new("/home/u/.omnifs"),
            &filesystem(),
            &spec(),
            "runtime",
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
