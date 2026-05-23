use omnifs_sdk::prelude::*;
use std::collections::HashMap;

use crate::{Config, State};

#[provider(
    metadata = "omnifs.provider.json",
    mounts(crate::categories::CategoryHandlers, crate::paper::PaperHandlers,)
)]
impl ArxivProvider {
    fn init(config: Config) -> (State, ProviderInfo, RequestedCapabilities) {
        (
            State {
                config,
                recent: HashMap::default(),
            },
            ProviderInfo {
                name: "arxiv-provider".to_string(),
                version: "0.1.0".to_string(),
                description: "arXiv recent submissions provider for omnifs".to_string(),
            },
            RequestedCapabilities::runtime_only(3600),
        )
    }
}
