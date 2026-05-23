use omnifs_sdk::Cx;
use omnifs_sdk::prelude::*;

use crate::events::timer_tick;
use crate::{Config, State};

#[provider(
    metadata = "omnifs.provider.json",
    mounts(
        crate::root::RootHandlers,
        crate::repo::RepoHandlers,
        crate::issues::IssueHandlers,
        crate::pulls::PullHandlers,
        crate::actions::ActionHandlers,
    )
)]
impl GithubProvider {
    fn init(_config: Config) -> (State, ProviderInfo, RequestedCapabilities) {
        (
            State {
                event_etags: hashbrown::HashMap::new(),
            },
            ProviderInfo {
                name: "github-provider".to_string(),
                version: "0.1.0".to_string(),
                description: "GitHub API provider for omnifs".to_string(),
            },
            RequestedCapabilities::with_git(60),
        )
    }

    async fn on_event(cx: Cx<State>, event: ProviderEvent) -> Result<Effects> {
        match event {
            ProviderEvent::TimerTick(_) => timer_tick(cx).await,
            _ => Ok(Effects::new()),
        }
    }
}
