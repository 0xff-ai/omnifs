//! `omnifs up` — container lifecycle: start.

use clap::Args;
use omnifs_creds::FileStore;

use crate::app_context::AppContext;
use crate::launch::{LaunchSpec, launch_runtime};
use crate::runtime::ContainerExtras;
use crate::session::GUEST_FUSE_MOUNT;

#[derive(Args, Debug, Clone, Default)]
pub struct UpArgs {
    /// Container image to run.
    ///
    /// Defaults to `OMNIFS_IMAGE`, then configured image, then the
    /// version-matched runtime image.
    #[arg(long)]
    pub image: Option<String>,
    /// Container name.
    ///
    /// Defaults to `OMNIFS_CONTAINER_NAME`, then configured session name, then
    /// `omnifs`.
    #[arg(long)]
    pub container_name: Option<String>,
}

impl UpArgs {
    pub async fn run(self) -> anyhow::Result<()> {
        use crate::paths::PathOverrides;

        let ctx = AppContext::resolve(PathOverrides::default(), self.container_name, self.image)?;
        let paths = ctx.paths();
        let runtime = ctx.runtime();
        let catalog = ctx.catalog();

        // Bail early before touching runtime state if there is nothing to mount.
        let configs = ctx.workspace().mounts()?;
        if configs.is_empty() {
            anyhow::bail!(
                "no mount configs found in {}; run `omnifs setup` for guided onboarding, or `omnifs init <provider>` to add one directly",
                paths.mounts_dir.display()
            );
        }

        let mounts_dir = paths.mounts_dir.clone();
        let store = Box::new(FileStore::new(&paths.credentials_file));

        crate::provider_bundle::ensure_release_bundle(&paths.providers_dir).await?;

        anstream::println!("Using mount configs from {}", mounts_dir.display());
        launch_runtime(
            LaunchSpec {
                runtime,
                runtime_home: &paths.config_dir,
                store,
                verb: "omnifs up",
                configs,
                extras: ContainerExtras::default(),
            },
            catalog,
        )
        .await?;

        anstream::println!(
            "✓ {GUEST_FUSE_MOUNT} is mounted inside `{}`",
            runtime.container_name()
        );
        anstream::println!();
        anstream::println!(
            "Run `{}` to open a shell inside the container and browse {GUEST_FUSE_MOUNT}.",
            crate::style::bold("omnifs shell"),
        );
        Ok(())
    }
}
