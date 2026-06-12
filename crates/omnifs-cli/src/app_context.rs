use crate::catalog::ProviderCatalog;
use crate::config::Config;
use crate::paths::{PathOverrides, Paths};
use crate::runtime_target::RuntimeTarget;
use crate::workspace::Workspace;

#[derive(Debug, Clone)]
pub(crate) struct AppContext {
    paths: Paths,
    config: Config,
    runtime: RuntimeTarget,
    catalog: ProviderCatalog,
    workspace: Workspace,
}

impl AppContext {
    pub(crate) fn resolve_default() -> anyhow::Result<Self> {
        Self::resolve(PathOverrides::default(), None, None)
    }

    pub(crate) fn resolve(
        path_overrides: PathOverrides,
        container_name: Option<String>,
        image: Option<String>,
    ) -> anyhow::Result<Self> {
        let (paths, config) = crate::paths::resolve_with_config(path_overrides)?;
        let runtime = RuntimeTarget::resolve(container_name, image, &config)?;
        let workspace = Workspace::new(paths.clone(), config.mounts.clone());
        let catalog = ProviderCatalog::for_dirs(&paths.mounts_dir, &paths.providers_dir);
        Ok(Self {
            paths,
            config,
            runtime,
            catalog,
            workspace,
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

    pub(crate) fn workspace(&self) -> &Workspace {
        &self.workspace
    }
}
