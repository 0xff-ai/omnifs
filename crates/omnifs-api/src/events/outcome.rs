use serde::{Deserialize, Serialize};
use strum::Display;

/// Stable machine outcome for terminal lifecycle events. `Display` matches
/// the serde wire form (`snake_case`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum InspectorOutcome {
    Ok,
    NotFound,
    Denied,
    InvalidInput,
    Timeout,
    Network,
    TooLarge,
    ProviderTrap,
    Internal,
}

/// Optional redacted detail attached to end events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutcomeFields {
    pub outcome: InspectorOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl OutcomeFields {
    pub fn ok() -> Self {
        Self {
            outcome: InspectorOutcome::Ok,
            message: None,
        }
    }

    pub fn with_outcome(outcome: InspectorOutcome) -> Self {
        Self {
            outcome,
            message: None,
        }
    }
}
