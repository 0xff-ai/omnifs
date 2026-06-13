//! `omnifs down` — container lifecycle: stop.

use clap::Args;

use crate::runtime::Runtime;
use crate::runtime_target::RuntimeTarget;

#[derive(Args, Debug, Clone, Default)]
pub struct DownArgs {
    /// Container name.
    ///
    /// Defaults to `OMNIFS_CONTAINER_NAME`, then configured session name, then
    /// `omnifs`.
    #[arg(long)]
    pub container_name: Option<String>,
    /// Force the Docker container path even on macOS.
    ///
    /// macOS defaults to tearing down the host-native daemon (NFS);
    /// `--isolated` selects the Docker container backend instead. On Linux the
    /// Docker path is always used and this flag has no effect.
    #[arg(long)]
    pub isolated: bool,
}

impl DownArgs {
    pub async fn run(self) -> anyhow::Result<()> {
        use crate::paths::PathOverrides;

        let DownArgs {
            container_name,
            isolated,
        } = self;
        let (paths, config) = crate::paths::resolve_with_config(PathOverrides::default())?;

        // macOS tears down the host-native NFS mount by default; `--isolated`
        // forces the Docker container path. Linux always uses Docker.
        let host_native = cfg!(target_os = "macos") && !isolated;
        if host_native {
            let state_dir = paths.cache_dir.join("nfs");
            let torn = crate::host_teardown::teardown_host_native(&state_dir)?;
            if torn == 0 {
                anstream::println!("No host-native omnifs mount is running.");
            } else {
                anstream::println!("✓ omnifs unmounted");
            }
            return Ok(());
        }

        let container_name = RuntimeTarget::resolve_container_name(container_name, &config)?;
        let remove_result = match Runtime::connect_docker() {
            Ok(runtime) => runtime.remove_existing(&container_name).await,
            Err(error) => Err(error),
        };
        remove_result?;
        anstream::println!("✓ Container `{container_name}` removed");
        Ok(())
    }
}
