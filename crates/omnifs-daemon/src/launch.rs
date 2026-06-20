//! Launch-time helpers for spawning the daemon from the CLI.
//!
//! Flag knowledge lives next to [`DaemonArgs`]; this module holds the typed
//! inputs the launcher supplies before argv serialization.

use std::net::SocketAddr;

use omnifs_home::Paths;

use crate::app::DaemonArgs;

/// Inputs for spawning a host-native daemon child from the CLI.
#[derive(Debug, Clone)]
pub struct NativeLaunchConfig {
    pub paths: Paths,
    pub listen: SocketAddr,
}

impl From<NativeLaunchConfig> for DaemonArgs {
    fn from(config: NativeLaunchConfig) -> Self {
        Self {
            config_dir: Some(config.paths.config_dir),
            cache_dir: Some(config.paths.cache_dir),
            listen: config.listen,
            host_native: true,
            nfs_port: 0,
            nfs_state_dir: None,
            nfs_trace: None,
            root_symlinks: false,
        }
    }
}
