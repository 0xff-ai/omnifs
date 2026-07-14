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

impl InspectorOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::NotFound => "not_found",
            Self::Denied => "denied",
            Self::InvalidInput => "invalid_input",
            Self::Timeout => "timeout",
            Self::Network => "network",
            Self::TooLarge => "too_large",
            Self::ProviderTrap => "provider_trap",
            Self::Internal => "internal",
        }
    }

    pub fn from_field(value: &str) -> Option<Self> {
        Some(match value {
            "ok" => Self::Ok,
            "not_found" => Self::NotFound,
            "denied" => Self::Denied,
            "invalid_input" => Self::InvalidInput,
            "timeout" => Self::Timeout,
            "network" => Self::Network,
            "too_large" => Self::TooLarge,
            "provider_trap" => Self::ProviderTrap,
            "internal" => Self::Internal,
            _ => return None,
        })
    }
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
