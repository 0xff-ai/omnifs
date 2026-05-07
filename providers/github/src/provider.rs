use omnifs_sdk::Cx;
use omnifs_sdk::prelude::*;

use crate::events::timer_tick;
use crate::{Config, State};

#[provider(mounts(
    crate::root::RootHandlers,
    crate::repo::RepoHandlers,
    crate::issues::IssueHandlers,
    crate::pulls::PullHandlers,
    crate::actions::ActionHandlers,
))]
impl GithubProvider {
    fn init(_config: Config) -> (State, ProviderInfo) {
        (
            State {
                event_etags: hashbrown::HashMap::new(),
                event_log: std::collections::VecDeque::with_capacity(crate::EVENT_LOG_CAPACITY),
            },
            ProviderInfo {
                name: "github-provider".to_string(),
                version: "0.1.0".to_string(),
                description: "GitHub API provider for omnifs".to_string(),
            },
        )
    }

    fn capabilities() -> RequestedCapabilities {
        RequestedCapabilities {
            domains: vec!["api.github.com".to_string()],
            auth_types: vec!["bearer-token".to_string()],
            max_memory_mb: 128,
            needs_git: true,
            needs_websocket: false,
            needs_streaming: false,
            refresh_interval_secs: 60,
        }
    }

    async fn on_event(cx: Cx<State>, event: ProviderEvent) -> Result<EventOutcome> {
        match event {
            ProviderEvent::TimerTick(_) => timer_tick(cx).await,
            _ => Ok(EventOutcome::new()),
        }
    }
}
