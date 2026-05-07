use omnifs_sdk::prelude::*;

use crate::{Config, State};

#[provider(mounts(crate::root::RootHandlers, crate::meta::MetaHandlers))]
impl IpfsProvider {
    fn init(config: Config) -> Result<(State, ProviderInfo)> {
        if config.api_base_url.trim().is_empty() {
            return Err(ProviderError::invalid_input(
                "api_base_url must not be empty",
            ));
        }
        Ok((
            State { config },
            ProviderInfo {
                name: "ipfs-provider".to_string(),
                version: "0.1.0".to_string(),
                description: "Read-only IPFS and IPNS browsing via the Kubo RPC API".to_string(),
            },
        ))
    }

    fn capabilities() -> RequestedCapabilities {
        RequestedCapabilities {
            domains: Vec::new(),
            auth_types: Vec::new(),
            max_memory_mb: 64,
            needs_git: false,
            needs_websocket: false,
            needs_streaming: false,
            refresh_interval_secs: 0,
        }
    }
}
