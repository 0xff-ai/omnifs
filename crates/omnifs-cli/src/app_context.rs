use crate::catalog::ProviderCatalog;
use crate::config::Config;
use crate::paths::{PathOverrides, Paths};
use crate::runtime_mode::RuntimeMode;
use crate::runtime_target::RuntimeTarget;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub(crate) struct AppContext {
    paths: Paths,
    config: Config,
    runtime: RuntimeTarget,
    catalog: ProviderCatalog,
}

impl AppContext {
    pub(crate) fn resolve_default() -> anyhow::Result<Self> {
        Self::from_options(RuntimeOptions::default())
    }

    pub(crate) fn resolve(
        path_overrides: PathOverrides,
        container_name: Option<String>,
        image: Option<String>,
    ) -> anyhow::Result<Self> {
        Self::from_options(RuntimeOptions {
            path_overrides,
            container_name,
            image,
            ..RuntimeOptions::default()
        })
    }

    pub(crate) fn resolve_with_runtime(
        path_overrides: PathOverrides,
        container_name: Option<String>,
        image: Option<String>,
        mode: Option<RuntimeMode>,
        mount_point: Option<PathBuf>,
    ) -> anyhow::Result<Self> {
        Self::from_options(RuntimeOptions {
            path_overrides,
            container_name,
            image,
            mode,
            mount_point,
            default_mode: RuntimeMode::Auto,
        })
    }

    pub(crate) fn resolve_dev(
        path_overrides: PathOverrides,
        container_name: Option<String>,
        image: Option<String>,
        mode: Option<RuntimeMode>,
        mount_point: Option<PathBuf>,
    ) -> anyhow::Result<Self> {
        Self::from_options(RuntimeOptions {
            path_overrides,
            container_name,
            image,
            mode,
            mount_point,
            default_mode: RuntimeMode::Docker,
        })
    }

    fn from_options(options: RuntimeOptions) -> anyhow::Result<Self> {
        let (paths, config) = Paths::resolve_with_config(options.path_overrides)?;
        let config = RuntimeConfigOverlay::new(config, options.mode, options.mount_point)
            .with_default_mode(options.default_mode)
            .into_config();
        let runtime = RuntimeTarget::resolve(options.container_name, options.image, &config)?;
        let catalog = ProviderCatalog::with_config(
            &paths.mounts_dir,
            &paths.providers_dir,
            &paths.config_file,
            config.mounts.clone(),
        );
        Ok(Self {
            paths,
            config,
            runtime,
            catalog,
        })
    }

    pub(crate) fn paths(&self) -> &Paths {
        &self.paths
    }

    pub(crate) fn config(&self) -> &Config {
        &self.config
    }

    pub(crate) fn runtime(&self) -> &RuntimeTarget {
        &self.runtime
    }

    pub(crate) fn catalog(&self) -> &ProviderCatalog {
        &self.catalog
    }
}

#[derive(Debug, Default)]
struct RuntimeOptions {
    path_overrides: PathOverrides,
    container_name: Option<String>,
    image: Option<String>,
    mode: Option<RuntimeMode>,
    mount_point: Option<PathBuf>,
    default_mode: RuntimeMode,
}

struct RuntimeConfigOverlay {
    config: Config,
}

impl RuntimeConfigOverlay {
    fn new(mut config: Config, mode: Option<RuntimeMode>, mount_point: Option<PathBuf>) -> Self {
        config.runtime.mode = mode.or(config.runtime.mode);
        config.runtime.mount_point = mount_point.or(config.runtime.mount_point);
        Self { config }
    }

    fn with_default_mode(mut self, mode: RuntimeMode) -> Self {
        self.config.runtime.mode = self.config.runtime.mode.or(Some(mode));
        self
    }

    fn into_config(self) -> Config {
        self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[allow(unsafe_code)] // env mutation is serialized by ENV_LOCK.
    fn with_home<F: FnOnce(&std::path::Path)>(f: F) {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = tempfile::tempdir().unwrap();
        let saved_home = std::env::var_os("OMNIFS_HOME");
        let saved_image = std::env::var_os(crate::session::ENV_IMAGE);
        let saved_container = std::env::var_os(crate::session::ENV_CONTAINER_NAME);

        unsafe {
            std::env::set_var("OMNIFS_HOME", temp.path());
            std::env::remove_var(crate::session::ENV_IMAGE);
            std::env::remove_var(crate::session::ENV_CONTAINER_NAME);
        }

        f(temp.path());

        unsafe {
            restore_env("OMNIFS_HOME", saved_home);
            restore_env(crate::session::ENV_IMAGE, saved_image);
            restore_env(crate::session::ENV_CONTAINER_NAME, saved_container);
        }
    }

    #[allow(unsafe_code)]
    unsafe fn restore_env(key: &str, value: Option<std::ffi::OsString>) {
        match value {
            Some(value) => unsafe { std::env::set_var(key, value) },
            None => unsafe { std::env::remove_var(key) },
        }
    }

    #[test]
    fn runtime_commands_use_persisted_mode() {
        with_home(|home| {
            std::fs::write(home.join("config.toml"), "[system]\nmode = \"native\"\n").unwrap();

            let ctx =
                AppContext::resolve_with_runtime(PathOverrides::default(), None, None, None, None)
                    .unwrap();

            assert!(matches!(ctx.runtime(), RuntimeTarget::Native(_)));
        });
    }

    #[test]
    fn runtime_command_mode_override_wins_over_config() {
        with_home(|home| {
            std::fs::write(home.join("config.toml"), "[system]\nmode = \"native\"\n").unwrap();

            let ctx = AppContext::resolve_with_runtime(
                PathOverrides::default(),
                None,
                None,
                Some(RuntimeMode::Docker),
                None,
            )
            .unwrap();

            assert!(matches!(ctx.runtime(), RuntimeTarget::Docker(_)));
        });
    }

    #[test]
    fn dev_defaults_to_docker_only_without_persisted_mode() {
        with_home(|_| {
            let ctx =
                AppContext::resolve_dev(PathOverrides::default(), None, None, None, None).unwrap();

            assert!(matches!(ctx.runtime(), RuntimeTarget::Docker(_)));
        });
    }

    #[test]
    fn dev_uses_persisted_mode_when_present() {
        with_home(|home| {
            std::fs::write(home.join("config.toml"), "[system]\nmode = \"native\"\n").unwrap();

            let ctx =
                AppContext::resolve_dev(PathOverrides::default(), None, None, None, None).unwrap();

            assert!(matches!(ctx.runtime(), RuntimeTarget::Native(_)));
        });
    }
}
