use omnifs_sdk::prelude::*;

use crate::doh::ResolverConfig;
use crate::{Config, State};

#[provider(
    metadata = "omnifs.provider.json",
    mounts(crate::root::RootHandlers, crate::segment::SegmentHandlers)
)]
impl DnsProvider {
    fn init(config: Config) -> Result<(State, ProviderInfo, RequestedCapabilities)> {
        let resolvers = ResolverConfig::from_config(config.default_resolver, config.resolvers)?;
        Ok((
            State { resolvers },
            ProviderInfo {
                name: "dns-provider".to_string(),
                version: "0.1.0".to_string(),
                description: "DNS record browsing via DNS-over-HTTPS".to_string(),
            },
            RequestedCapabilities::empty(),
        ))
    }
}
