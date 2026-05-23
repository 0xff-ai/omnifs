use crate::catalog::ProviderCatalog;
use crate::config::Config;
use crate::paths::{PathOverrides, Paths};
use crate::runtime_selection::RuntimeSelection;

#[derive(Debug, Clone)]
pub(crate) struct AppContext {
    paths: Paths,
    config: Config,
    runtime: RuntimeSelection,
    catalog: ProviderCatalog,
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
        let (paths, config) = Paths::resolve_with_config(path_overrides)?;
        let runtime = RuntimeSelection::resolve(container_name, image, &config)?;
        let catalog = ProviderCatalog::new(&paths.mounts_dir, &paths.providers_dir);
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

    pub(crate) fn runtime(&self) -> &RuntimeSelection {
        &self.runtime
    }

    pub(crate) fn catalog(&self) -> &ProviderCatalog {
        &self.catalog
    }
}
