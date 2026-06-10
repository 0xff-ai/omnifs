//! `omnifs up` — container lifecycle: start.

use std::path::PathBuf;

use clap::Args;

use crate::app_context::AppContext;
use crate::launch::{LaunchSpec, launch_session};
use crate::runtime::ContainerExtras;
use crate::session::{CredsBackend, HOST_FUSE_MOUNT};

#[derive(Args, Debug, Clone, Default)]
pub struct UpArgs {
    /// Override the directory holding user-scope mount configs.
    ///
    /// Defaults to `OMNIFS_MOUNTS_DIR`, then the default config mounts dir.
    #[arg(long)]
    pub mounts_dir: Option<PathBuf>,
    /// Container image to run.
    ///
    /// Defaults to `OMNIFS_IMAGE`, then configured image, then the
    /// version-matched runtime image.
    #[arg(long)]
    pub image: Option<String>,
    /// Directory holding provider WASM components for host-side metadata.
    ///
    /// Defaults to `OMNIFS_PROVIDERS_DIR`, then the default config providers dir.
    #[arg(long)]
    pub providers_dir: Option<PathBuf>,
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

        let ctx = AppContext::resolve(
            PathOverrides {
                mounts_dir: self.mounts_dir,
                providers_dir: self.providers_dir,
                ..Default::default()
            },
            self.container_name,
            self.image,
        )?;
        let paths = ctx.paths();
        let runtime = ctx.runtime();
        let catalog = ctx.catalog();

        // Bail early before touching session state if there is nothing to mount.
        let configs = catalog.session_mount_configs()?;
        if configs.is_empty() {
            anyhow::bail!(
                "no mount configs found in {}; run `omnifs setup` for guided onboarding, or `omnifs init <provider>` to add one directly",
                paths.mounts_dir.display()
            );
        }

        let mounts_dir = paths.mounts_dir.clone();
        let store = CredsBackend::auto(&paths.credentials_file, true);

        anstream::println!("Using mount configs from {}", mounts_dir.display());
        launch_session(
            LaunchSpec {
                runtime,
                credentials_file: &paths.credentials_file,
                store,
                verb: "omnifs up",
                configs,
                extras: ContainerExtras::default(),
            },
            catalog,
        )
        .await?;

        anstream::println!(
            "✓ {HOST_FUSE_MOUNT} is mounted inside `{}`",
            runtime.container_name()
        );
        anstream::println!();
        anstream::println!(
            "Run `{}` to open a shell inside the container and browse {HOST_FUSE_MOUNT}.",
            crate::style::bold("omnifs shell"),
        );
        Ok(())
    }
}
