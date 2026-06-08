use crate::config::Config;
use crate::container_name::ContainerName;
use crate::image_ref::ImageRef;
use crate::runtime::Runtime;
use crate::runtime_mode::RuntimeMode;
use crate::session::{CONTAINER_NAME, ENV_CONTAINER_NAME, ENV_IMAGE, IMAGE, env_string};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RuntimeTarget {
    Docker(DockerTarget),
    Native(NativeTarget),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DockerTarget {
    container_name: ContainerName,
    image: ImageRef,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeTarget {
    session_name: ContainerName,
    mount_point: PathBuf,
}

impl RuntimeTarget {
    pub(crate) fn resolve(
        container_name: Option<String>,
        image: Option<String>,
        config: &Config,
    ) -> anyhow::Result<Self> {
        match config.runtime.mode.unwrap_or_default() {
            RuntimeMode::Auto | RuntimeMode::Docker => {
                DockerTarget::resolve(container_name, image, config).map(Self::Docker)
            },
            RuntimeMode::Native => {
                NativeTarget::resolve(container_name, None, config).map(Self::Native)
            },
        }
    }

    pub(crate) fn resolve_container_name(
        container_name: Option<String>,
        config: &Config,
    ) -> anyhow::Result<ContainerName> {
        resolve_container_name(container_name, config)
    }

    pub(crate) fn session_name(&self) -> &ContainerName {
        match self {
            Self::Docker(target) => target.container_name(),
            Self::Native(target) => target.session_name(),
        }
    }

    pub(crate) fn mount_label(&self) -> String {
        match self {
            Self::Docker(_) => crate::session::HOST_FUSE_MOUNT.to_string(),
            Self::Native(target) => target.mount_point().display().to_string(),
        }
    }

    pub(crate) fn runtime_label(&self) -> String {
        match self {
            Self::Docker(target) => format!("docker ({})", target.container_name()),
            Self::Native(_) => "native".to_string(),
        }
    }
}

impl DockerTarget {
    pub(crate) fn resolve(
        container_name: Option<String>,
        image: Option<String>,
        config: &Config,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            container_name: resolve_container_name(container_name, config)?,
            image: resolve_image(image, config)?,
        })
    }

    pub(crate) fn container_name(&self) -> &ContainerName {
        &self.container_name
    }

    pub(crate) fn image(&self) -> &ImageRef {
        &self.image
    }

    pub(crate) async fn connect_ready(&self, command: &'static str) -> anyhow::Result<Runtime> {
        Runtime::connect_ready(self, command).await
    }
}

impl NativeTarget {
    pub(crate) fn resolve(
        session_name: Option<String>,
        mount_point: Option<PathBuf>,
        config: &Config,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            session_name: resolve_container_name(session_name, config)?,
            mount_point: resolve_mount_point(mount_point, config),
        })
    }

    pub(crate) fn session_name(&self) -> &ContainerName {
        &self.session_name
    }

    pub(crate) fn mount_point(&self) -> &Path {
        &self.mount_point
    }
}

fn resolve_container_name(
    container_name: Option<String>,
    config: &Config,
) -> anyhow::Result<ContainerName> {
    let container_name = container_name
        .or_else(|| env_string(ENV_CONTAINER_NAME))
        .or_else(|| config.container_name.clone())
        .unwrap_or_else(|| CONTAINER_NAME.to_string());
    ContainerName::new(container_name)
}

fn resolve_image(image: Option<String>, config: &Config) -> anyhow::Result<ImageRef> {
    let image = image
        .or_else(|| env_string(ENV_IMAGE))
        .or_else(|| config.image.clone())
        .unwrap_or_else(|| IMAGE.to_string());
    Ok(ImageRef::new(image)?)
}

fn resolve_mount_point(mount_point: Option<PathBuf>, config: &Config) -> PathBuf {
    mount_point
        .or_else(|| config.runtime.mount_point.clone())
        .unwrap_or_else(default_native_mount_point)
}

fn default_native_mount_point() -> PathBuf {
    std::env::var_os("HOME").map_or_else(
        || PathBuf::from("/tmp/OmniFS"),
        |home| PathBuf::from(home).join("OmniFS"),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

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

        for (key, value) in vars {
            match value {
                Some(v) => unsafe { std::env::set_var(key, v) },
                None => unsafe { std::env::remove_var(key) },
            }
        }

        f();

        for (key, original) in &saved {
            match original {
                Some(v) => unsafe { std::env::set_var(key, v) },
                None => unsafe { std::env::remove_var(key) },
            }
        }
    }

    #[test]
    fn resolve_uses_config_image_when_env_unset() {
        with_env(&[(ENV_IMAGE, None), (ENV_CONTAINER_NAME, None)], || {
            let config = Config {
                image: Some("ghcr.io/example/custom:1.2.3".into()),
                ..Default::default()
            };
            let RuntimeTarget::Docker(target) =
                RuntimeTarget::resolve(None, None, &config).unwrap()
            else {
                panic!("default runtime should resolve to docker");
            };
            assert_eq!(target.image().as_str(), "ghcr.io/example/custom:1.2.3");
        });
    }

    #[test]
    fn resolve_prefers_env_image_over_config() {
        with_env(
            &[
                (ENV_IMAGE, Some("ghcr.io/example/env:9.9.9")),
                (ENV_CONTAINER_NAME, None),
            ],
            || {
                let config = Config {
                    image: Some("ghcr.io/example/config:1.0.0".into()),
                    ..Default::default()
                };
                let RuntimeTarget::Docker(target) =
                    RuntimeTarget::resolve(None, None, &config).unwrap()
                else {
                    panic!("default runtime should resolve to docker");
                };
                assert_eq!(target.image().as_str(), "ghcr.io/example/env:9.9.9");
            },
        );
    }

    #[test]
    fn resolve_prefers_explicit_image_over_env_and_config() {
        with_env(
            &[
                (ENV_IMAGE, Some("ghcr.io/example/env:9.9.9")),
                (ENV_CONTAINER_NAME, None),
            ],
            || {
                let config = Config {
                    image: Some("ghcr.io/example/config:1.0.0".into()),
                    ..Default::default()
                };
                let RuntimeTarget::Docker(target) =
                    RuntimeTarget::resolve(None, Some("ghcr.io/example/cli:2.0.0".into()), &config)
                        .unwrap()
                else {
                    panic!("default runtime should resolve to docker");
                };
                assert_eq!(target.image().as_str(), "ghcr.io/example/cli:2.0.0");
            },
        );
    }

    #[test]
    fn resolve_container_name_prefers_env_over_config() {
        with_env(
            &[(ENV_IMAGE, None), (ENV_CONTAINER_NAME, Some("omnifs-env"))],
            || {
                let config = Config {
                    container_name: Some("omnifs-config".into()),
                    ..Default::default()
                };
                let container_name = RuntimeTarget::resolve_container_name(None, &config).unwrap();
                assert_eq!(container_name.as_str(), "omnifs-env");
            },
        );
    }

    #[test]
    fn resolve_container_name_prefers_explicit_over_env_and_config() {
        with_env(
            &[(ENV_IMAGE, None), (ENV_CONTAINER_NAME, Some("omnifs-env"))],
            || {
                let config = Config {
                    container_name: Some("omnifs-config".into()),
                    ..Default::default()
                };
                let container_name =
                    RuntimeTarget::resolve_container_name(Some("omnifs-cli".into()), &config)
                        .unwrap();
                assert_eq!(container_name.as_str(), "omnifs-cli");
            },
        );
    }

    #[test]
    fn resolve_uses_config_mode() {
        let config = Config {
            runtime: crate::config::ConfigRuntime {
                mode: Some(RuntimeMode::Native),
                ..Default::default()
            },
            ..Default::default()
        };
        let target = RuntimeTarget::resolve(None, None, &config).unwrap();
        assert!(matches!(target, RuntimeTarget::Native(_)));
    }

    #[test]
    fn resolve_defaults_auto_to_docker_target() {
        let target = RuntimeTarget::resolve(None, None, &Config::default()).unwrap();
        assert!(matches!(target, RuntimeTarget::Docker(_)));
    }
}
