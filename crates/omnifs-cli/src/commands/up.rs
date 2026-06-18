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
    /// Force the Docker container path even on macOS.
    ///
    /// macOS defaults to a host-native daemon (NFS); `--isolated` selects the
    /// Docker container backend instead. On Linux the Docker path is always
    /// used and this flag has no effect.
    #[arg(long)]
    pub isolated: bool,
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

        // macOS serves the mount host-native over NFS by default; `--isolated`
        // forces the Docker container path. Linux always uses Docker.
        let host_native = cfg!(target_os = "macos") && !self.isolated;
        let mount_point = paths.config_dir.join("mnt");
        let cache_dir = paths.cache_dir.clone();

        anstream::println!("Using mount configs from {}", mounts_dir.display());
        launch_runtime(
            LaunchSpec {
                runtime,
                runtime_home: &paths.config_dir,
                store,
                verb: "omnifs up",
                configs,
                extras: ContainerExtras::default(),
                host_native,
                mount_point: mount_point.clone(),
                cache_dir,
            },
            catalog,
        )
        .await?;

        if host_native {
            anstream::println!("✓ omnifs is mounted at {}", mount_point.display());
            anstream::println!();
            anstream::println!(
                "Browse it directly: `{}`",
                crate::style::bold(format!("ls {}", mount_point.display())),
            );
        } else {
            anstream::println!(
                "✓ {GUEST_FUSE_MOUNT} is mounted inside `{}`",
                runtime.container_name()
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
