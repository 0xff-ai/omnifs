use omnifs_sdk::Cx;
use omnifs_sdk::http::HttpEndpoint;
use omnifs_sdk::prelude::*;

use crate::api::ApiBase;
use crate::events::timer_tick;
use crate::{Config, EventCheckpoint, State};

#[provider(mounts(
    crate::system::SystemHandlers,
    crate::containers::ContainerHandlers,
    crate::compose::ComposeHandlers,
))]
impl DockerProvider {
    fn init(config: Config) -> (State, ProviderInfo) {
        let endpoint = HttpEndpoint::parse(&config.endpoint);
        (
            State {
                api: ApiBase::new(endpoint),
                events: EventCheckpoint::default(),
                config,
            },
            ProviderInfo {
                name: "docker-provider".to_string(),
                version: "0.1.0".to_string(),
                description: "Docker daemon provider for omnifs".to_string(),
            },
        )
    }

    fn capabilities() -> RequestedCapabilities {
        RequestedCapabilities {
            domains: Vec::new(),
            unix_sockets: vec!["/var/run/docker.sock".to_string()],
            auth_types: Vec::new(),
            max_memory_mb: 64,
            needs_git: false,
            needs_websocket: false,
            needs_streaming: false,
            // Five seconds tracks a developer's interactive expectation
            // ("did the container come up yet?") without flooding the
            // daemon with /events polls. A real interactive shell can
            // re-list manually if it wants faster reaction.
            refresh_interval_secs: 5,
        }
    }

    async fn on_event(cx: Cx<State>, event: ProviderEvent) -> Result<Effects> {
        match event {
            ProviderEvent::TimerTick(_) => timer_tick(cx).await,
            _ => Ok(Effects::new()),
        }
    }
}
