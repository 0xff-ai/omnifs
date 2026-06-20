//! `omnifs up`: daemon lifecycle start.

use clap::Args;
use omnifs_creds::FileStore;

use crate::launch::{LaunchSpec, launch_runtime};
use crate::launch_backend::LaunchBackend;
use crate::runtime::ContainerExtras;
use crate::session::GUEST_FUSE_MOUNT;
use crate::workspace::Workspace;

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

        let workspace = Workspace::resolve(PathOverrides::default())?;
        let config = workspace.config()?;
        let paths = workspace.paths();
        let backend = LaunchBackend::resolve(&config, self.container_name, self.image)?;
        let backend_is_native = backend.is_native();
        let docker_target = backend.docker_target().cloned();
        let catalog = workspace.catalog();

        // Bail early before touching runtime state if there is nothing to mount.
        let configs = workspace.mounts()?;
        if configs.is_empty() {
            anyhow::bail!(
                "no mount configs found in {}; run `omnifs setup` for guided onboarding, or `omnifs init <provider>` to add one directly",
                paths.mounts_dir.display()
            );
        }

        let mounts_dir = paths.mounts_dir.clone();
        let store = Box::new(FileStore::new(&paths.credentials_file));

        crate::provider_bundle::install_embedded_bundle(&paths.providers_dir)?;

        // The runtime backend is a machine property recorded by `omnifs setup`;
        // `up` reads it. Host-native serves a host mount; Docker serves FUSE
        // inside the container.
        // Contract pre-flight: classify any provider contract changes and
        // auto-migrate additive-only ones before the daemon sees the specs.
        // Breaking changes and capability/auth deltas are hard errors here.
        crate::contract_preflight::run_preflight(
            &paths.mounts_dir,
            &paths.providers_dir,
            &configs,
        )?;

        anstream::println!("Using mount configs from {}", mounts_dir.display());
        launch_runtime(
            LaunchSpec {
                backend,
                paths,
                store,
                verb: "omnifs up",
                configs,
                extras: ContainerExtras::default(),
            },
            catalog,
        )
        .await?;

        if backend_is_native {
            anstream::println!();
            if let Ok(status) = workspace.daemon().status().await {
                anstream::println!(
                    "Browse it directly: `{}`",
                    crate::style::bold(format!("ls {}", status.mount_point.display())),
                );
            }
        } else if let Some(docker_target) = docker_target {
            anstream::println!(
                "✓ {GUEST_FUSE_MOUNT} is mounted inside `{}`",
                docker_target.container_name()
            );
            anstream::println!();
            anstream::println!(
                "Run `{}` to open a shell inside the container and browse {GUEST_FUSE_MOUNT}.",
                crate::style::bold("omnifs shell"),
            );
        }
        Ok(())
    }
}
