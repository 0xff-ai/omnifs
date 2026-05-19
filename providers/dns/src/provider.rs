use omnifs_sdk::prelude::*;

use crate::doh::ResolverConfig;
use crate::{Config, State};

#[provider(mounts(crate::root::RootHandlers, crate::segment::SegmentHandlers))]
impl DnsProvider {
    fn init(config: Config) -> Result<(State, ProviderInfo)> {
        let resolvers = ResolverConfig::from_config(config.default_resolver, config.resolvers)?;
        Ok((
            State { resolvers },
            ProviderInfo {
                name: "dns-provider".to_string(),
                version: "0.1.0".to_string(),
                description: "DNS record browsing via DNS-over-HTTPS".to_string(),
            },
        ))
    }

    fn capabilities() -> RequestedCapabilities {
        RequestedCapabilities {
            domains: Vec::new(),
            unix_sockets: Vec::new(),
            auth_types: vec![],
            max_memory_mb: 32,
            needs_git: false,
            needs_websocket: false,
            needs_streaming: false,
            refresh_interval_secs: 0,
        }
    }
}
