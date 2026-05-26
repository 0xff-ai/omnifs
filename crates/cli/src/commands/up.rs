//! `omnifs up` — container lifecycle: start.

use std::path::PathBuf;

use clap::Args;

use crate::app_context::AppContext;
use crate::runtime::{ContainerExtras, Runtime};
use crate::session::{HOST_FUSE_MOUNT, Session, discover_mounts, open_store};

#[derive(Args, Debug, Clone, Default)]
pub struct UpArgs {
    /// Override the directory holding user-scope mount configs.
    ///
    /// Defaults to `OMNIFS_MOUNTS_DIR`, then the default config mounts dir.
    #[arg(long)]
    pub mounts_dir: Option<PathBuf>,
    /// Container image to run.
    ///
    /// Defaults to `OMNIFS_IMAGE`, then the version-matched runtime image.
    #[arg(long)]
    pub image: Option<String>,
    /// Directory holding provider WASM components for host-side metadata.
    ///
    /// Defaults to `OMNIFS_PROVIDERS_DIR`, then the default config providers dir.
    #[arg(long)]
    pub providers_dir: Option<PathBuf>,
    /// Container name.
    ///
    /// Defaults to `OMNIFS_CONTAINER_NAME`, then `omnifs`.
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
        let configs = discover_mounts(catalog)?;
        if configs.is_empty() {
            anyhow::bail!(
                "no mount configs found in {}; run `omnifs setup` for guided onboarding, or `omnifs init <provider>` to add one directly",
                paths.mounts_dir.display()
            );
        }

        let session = Session::prepare(runtime.container_name(), &paths.credentials_file)?;
        let mut cleanup = session.cleanup_on_drop();
        anstream::println!("Preparing session at {}", session.root().display());
        anstream::println!("Using mount configs from {}", paths.mounts_dir.display());
        let store = open_store(&paths.credentials_file, true);
        anstream::println!("Materializing mount configs and credentials");
        session.populate(&configs, catalog, store.as_ref())?;
        anstream::println!("✓ Materialized {} mount(s)", configs.len());

        let runtime_handle = Runtime::connect_ready(runtime, "omnifs up").await?;

        runtime_handle
            .launch_container(&session, ContainerExtras::default())
            .await?;

        runtime_handle.wait_for_fuse_mount().await?;
        runtime_handle.verify_status(&configs).await?;
        cleanup.disarm();
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
