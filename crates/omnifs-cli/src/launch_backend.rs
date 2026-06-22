use std::fmt;

use thiserror::Error;

use crate::config::{Config, ConfiguredBackend};
use crate::session::{CONTAINER_NAME, ENV_CONTAINER_NAME, ENV_IMAGE, IMAGE, env_string};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ContainerName(String);

impl ContainerName {
    pub(crate) fn new(name: impl Into<String>) -> anyhow::Result<Self> {
        let name = name.into();
        validate_container_name(&name)?;
        Ok(Self(name))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ContainerName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for ContainerName {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

fn validate_container_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() {
        anyhow::bail!("container name must not be empty");
    }
    if name.len() > 64 {
        anyhow::bail!("container name must be at most 64 characters");
    }
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        anyhow::bail!("container name must not be empty");
    };
    if !first.is_ascii_alphanumeric() {
        anyhow::bail!("container name must start with an ASCII letter or digit");
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')) {
        anyhow::bail!("container name may only contain ASCII letters, digits, _, ., and -");
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImageRef(String);

impl ImageRef {
    pub(crate) fn new(image: impl Into<String>) -> Result<Self, ImageRefError> {
        let image = image.into();
        if image.trim().is_empty() {
            return Err(ImageRefError);
        }
        Ok(Self(image))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ImageRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for ImageRef {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

#[derive(Debug, Clone, Copy, Error, PartialEq, Eq)]
#[error("image reference must not be empty")]
pub(crate) struct ImageRefError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DockerTarget {
    container_name: ContainerName,
    image: ImageRef,
}

impl DockerTarget {
    pub(crate) fn new(container_name: String, image: String) -> anyhow::Result<Self> {
        Ok(Self {
            container_name: ContainerName::new(container_name)?,
            image: ImageRef::new(image)?,
        })
    }

    pub(crate) fn resolve(
        container_name: Option<String>,
        image: Option<String>,
        config: &Config,
    ) -> anyhow::Result<Self> {
        let container_name = Self::resolve_container_name(container_name, config)?;
        let image = Self::resolve_image(image, config)?;
        Ok(Self {
            container_name,
            image,
        })
    }

    pub(crate) fn from_config(config: &Config) -> anyhow::Result<Self> {
        Ok(Self {
            container_name: Self::container_name_from_config(config)?,
            image: Self::image_from_config(config)?,
        })
    }

    pub(crate) fn resolve_container_name(
        container_name: Option<String>,
        config: &Config,
    ) -> anyhow::Result<ContainerName> {
        let container_name = container_name
            .or_else(|| env_string(ENV_CONTAINER_NAME))
            .or_else(|| config.container_name.clone())
            .unwrap_or_else(|| CONTAINER_NAME.to_string());
        ContainerName::new(container_name)
    }

    pub(crate) fn container_name(&self) -> &ContainerName {
        &self.container_name
    }

    pub(crate) fn image(&self) -> &ImageRef {
        &self.image
    }

    fn resolve_image(image: Option<String>, config: &Config) -> anyhow::Result<ImageRef> {
        let image = image
            .or_else(|| env_string(ENV_IMAGE))
            .or_else(|| config.image.clone())
            .unwrap_or_else(|| IMAGE.to_string());
        Ok(ImageRef::new(image)?)
    }

    fn container_name_from_config(config: &Config) -> anyhow::Result<ContainerName> {
        let container_name = config
            .container_name
            .clone()
            .unwrap_or_else(|| CONTAINER_NAME.to_string());
        ContainerName::new(container_name)
    }

    fn image_from_config(config: &Config) -> anyhow::Result<ImageRef> {
        let image = config.image.clone().unwrap_or_else(|| IMAGE.to_string());
        Ok(ImageRef::new(image)?)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LaunchBackend {
    Native,
    Docker(DockerTarget),
}

impl LaunchBackend {
    pub(crate) fn from_config(config: &Config) -> anyhow::Result<Self> {
        match config.backend() {
            ConfiguredBackend::Native => Ok(Self::Native),
            ConfiguredBackend::Docker => Ok(Self::Docker(DockerTarget::from_config(config)?)),
        }
    }

    pub(crate) fn resolve(
        config: &Config,
        container_name: Option<String>,
        image: Option<String>,
    ) -> anyhow::Result<Self> {
        match config.backend() {
            ConfiguredBackend::Native => Ok(Self::Native),
            ConfiguredBackend::Docker => Ok(Self::Docker(DockerTarget::resolve(
                container_name,
                image,
                config,
            )?)),
        }
    }

    pub(crate) fn docker(docker: DockerTarget) -> Self {
        Self::Docker(docker)
    }

    pub(crate) fn default_docker() -> anyhow::Result<Self> {
        Ok(Self::Docker(DockerTarget::new(
            CONTAINER_NAME.to_string(),
            IMAGE.to_string(),
        )?))
    }

    pub(crate) fn is_docker(&self) -> bool {
        matches!(self, Self::Docker(_))
    }
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

        // SAFETY: ENV_LOCK is held for the entire duration of this call,
        // so no other thread is reading or writing the environment concurrently.
        for (key, value) in vars {
            match value {
                Some(v) => unsafe { std::env::set_var(key, v) },
                None => unsafe { std::env::remove_var(key) },
            }
        }

        f();

        // SAFETY: ENV_LOCK is still held; restoring the saved values is subject
        // to the same serialization guarantee as the writes above.
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
            let target = DockerTarget::resolve(None, None, &config).unwrap();
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
                let target = DockerTarget::resolve(None, None, &config).unwrap();
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
                let target =
                    DockerTarget::resolve(None, Some("ghcr.io/example/cli:2.0.0".into()), &config)
                        .unwrap();
                assert_eq!(target.image().as_str(), "ghcr.io/example/cli:2.0.0");
            },
        );
    }

    #[test]
    fn from_config_ignores_env_docker_target_overrides() {
        with_env(
            &[
                (ENV_IMAGE, Some("ghcr.io/example/env:9.9.9")),
                (ENV_CONTAINER_NAME, Some("omnifs-env")),
            ],
            || {
                let config = Config {
                    system: crate::config::ConfigSystem {
                        runtime: Some(ConfiguredBackend::Docker),
                        ..Default::default()
                    },
                    image: Some("ghcr.io/example/config:1.0.0".into()),
                    container_name: Some("omnifs-config".into()),
                };
                let backend = LaunchBackend::from_config(&config).unwrap();
                let LaunchBackend::Docker(target) = backend else {
                    panic!("expected docker backend");
                };
                assert_eq!(target.image().as_str(), "ghcr.io/example/config:1.0.0");
                assert_eq!(target.container_name().as_str(), "omnifs-config");
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
                let container_name = DockerTarget::resolve_container_name(None, &config).unwrap();
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
                    DockerTarget::resolve_container_name(Some("omnifs-cli".into()), &config)
                        .unwrap();
                assert_eq!(container_name.as_str(), "omnifs-cli");
            },
        );
    }
}
