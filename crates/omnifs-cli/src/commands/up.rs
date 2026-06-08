//! `omnifs up` — runtime lifecycle: start.

use std::path::PathBuf;

use clap::Args;

use crate::app_context::AppContext;
use crate::native_runtime;
use crate::provider_artifacts::NativeArtifacts;
use crate::runtime::{ContainerExtras, GUEST_INSPECTOR_PORT};
use crate::runtime_mode::RuntimeMode;
use crate::runtime_target::RuntimeTarget;
use crate::session::{CredsBackend, HOST_FUSE_MOUNT, Session};

#[derive(Args, Debug, Clone, Default)]
pub struct UpArgs {
    /// Override the legacy directory holding per-mount JSON configs.
    #[arg(long, hide = true)]
    pub mounts_dir: Option<PathBuf>,
    /// Container image to run.
    ///
    /// Defaults to `OMNIFS_IMAGE`, then configured image, then the
    /// version-matched runtime image.
    #[arg(long)]
    pub image: Option<String>,
    /// Runtime launch mode.
    ///
    /// `auto` currently resolves to the Docker frontend.
    #[arg(long, value_enum)]
    pub mode: Option<RuntimeMode>,
    /// Host mount point for native mode.
    ///
    /// Defaults to `[system].mount_point`, then `~/OmniFS`.
    #[arg(long)]
    pub mount_point: Option<PathBuf>,
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

        let ctx = AppContext::resolve_with_runtime(
            PathOverrides {
                mounts_dir: self.mounts_dir,
                providers_dir: self.providers_dir,
                ..Default::default()
            },
            self.container_name,
            self.image,
            self.mode,
            self.mount_point,
        )?;
        let paths = ctx.paths();
        let runtime = ctx.runtime();
        let catalog = ctx.catalog();
        let configs = catalog.session_mount_configs()?;
        if configs.is_empty() {
            anyhow::bail!(
                "no mount configs found in {}; run `omnifs setup` for guided onboarding, or `omnifs init <provider>` to add one directly",
                paths.config_file.display()
            );
        }
        if matches!(runtime, RuntimeTarget::Native(_)) {
            NativeArtifacts::discover(paths).ensure_for_launch(&configs)?;
        }

        let session = Session::prepare(runtime.session_name(), &paths.credentials_file)?;
        let mut cleanup = session.cleanup_on_drop();
        anstream::println!("Preparing session at {}", session.root().display());
        anstream::println!("Using mount configs from {}", paths.config_file.display());
        let store = CredsBackend::auto(&paths.credentials_file, true);
        anstream::println!("Materializing mount configs and credentials");
        session.populate(&configs, catalog, store.as_ref())?;
        anstream::println!("✓ Materialized {} mount(s)", configs.len());

        match runtime {
            RuntimeTarget::Docker(target) => {
                let runtime_handle = target.connect_ready("omnifs up").await?;
                runtime_handle
                    .launch_container(
                        &session,
                        ContainerExtras {
                            tcp_ports: vec![GUEST_INSPECTOR_PORT],
                            ..ContainerExtras::default()
                        },
                    )
                    .await?;

                runtime_handle.wait_for_fuse_mount().await?;
                runtime_handle.verify_status(&configs).await?;
                cleanup.disarm();
                anstream::println!(
                    "✓ {HOST_FUSE_MOUNT} is mounted inside `{}`",
                    target.container_name()
                );
                anstream::println!();
                anstream::println!(
                    "Run `{}` to open a shell inside the container and browse {HOST_FUSE_MOUNT}.",
                    crate::style::bold("omnifs shell"),
                );
            },
            RuntimeTarget::Native(target) => {
                native_runtime::launch(paths, target, &session)?;
                cleanup.disarm();
                anstream::println!();
                anstream::println!(
                    "Run `{}` to open a shell at the native mount.",
                    crate::style::bold("omnifs shell"),
                );
            },
        }
        Ok(())
    }
}
