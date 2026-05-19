use omnifs_sdk::prelude::*;

use crate::{Config, State};

#[provider(mounts(
    crate::root::RootHandlers,
    crate::teams::TeamHandlers,
    crate::issues::IssueHandlers,
    crate::issue_subtree::IssueFileHandlers,
))]
impl LinearProvider {
    fn init(config: Config) -> (State, ProviderInfo) {
        (
            State { config },
            ProviderInfo {
                name: "linear-provider".to_string(),
                version: "0.1.0".to_string(),
                description: "Linear provider for omnifs".to_string(),
            },
        )
    }

    fn capabilities() -> RequestedCapabilities {
        RequestedCapabilities {
            domains: vec!["api.linear.app".to_string()],
            unix_sockets: Vec::new(),
            auth_types: vec!["api-key-header".to_string()],
            max_memory_mb: 128,
            needs_git: false,
            needs_websocket: false,
            needs_streaming: false,
            refresh_interval_secs: 120,
        }
    }
}
