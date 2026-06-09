use crate::config::Config;
use crate::container_name::ContainerName;
use crate::image_ref::ImageRef;
use crate::session::{CONTAINER_NAME, ENV_CONTAINER_NAME, ENV_IMAGE, IMAGE, env_string};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeTarget {
    container_name: ContainerName,
    image: ImageRef,
}

impl RuntimeTarget {
    pub(crate) fn resolve(
        container_name: Option<String>,
        image: Option<String>,
        config: &Config,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            container_name: Self::resolve_container_name(container_name, config)?,
            image: Self::resolve_image(image, config)?,
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
            let target = RuntimeTarget::resolve(None, None, &config).unwrap();
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
                let target = RuntimeTarget::resolve(None, None, &config).unwrap();
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
                    RuntimeTarget::resolve(None, Some("ghcr.io/example/cli:2.0.0".into()), &config)
                        .unwrap();
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
}
