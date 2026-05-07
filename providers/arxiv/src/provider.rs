use omnifs_sdk::prelude::*;

use crate::{Config, State};

#[provider(mounts(
    crate::root::RootHandlers,
    crate::categories::CategoryHandlers,
    crate::authors::AuthorHandlers,
    crate::search::SearchHandlers,
    crate::papers::PaperHandlers,
))]
impl ArxivProvider {
    fn init(config: Config) -> (State, ProviderInfo) {
        (
            State { config },
            ProviderInfo {
                name: "arxiv-provider".to_string(),
                version: "0.1.0".to_string(),
                description: "arXiv provider for omnifs".to_string(),
            },
        )
    }

    fn capabilities() -> RequestedCapabilities {
        RequestedCapabilities {
            domains: vec!["export.arxiv.org".to_string(), "arxiv.org".to_string()],
            auth_types: vec![],
            max_memory_mb: 64,
            needs_git: false,
            needs_websocket: false,
            needs_streaming: false,
            refresh_interval_secs: 3600,
        }
    }
}
