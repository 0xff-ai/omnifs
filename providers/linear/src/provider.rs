use omnifs_sdk::prelude::*;

use crate::{Config, State};

#[provider(
    metadata = "omnifs.provider.json",
    mounts(
        crate::root::RootHandlers,
        crate::teams::TeamHandlers,
        crate::issues::IssueHandlers,
        crate::issue_subtree::IssueFileHandlers,
    )
)]
impl LinearProvider {
    fn init(config: Config) -> (State, ProviderInfo, RequestedCapabilities) {
        (
            State { config },
            ProviderInfo {
                name: "linear-provider".to_string(),
                version: "0.1.0".to_string(),
                description: "Linear provider for omnifs".to_string(),
            },
            RequestedCapabilities::runtime_only(120),
        )
    }
}
