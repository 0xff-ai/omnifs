//! Typed local control-plane wire types.

use crate::DaemonStatus;
use serde::{Deserialize, Serialize};

/// The only control protocol version understood by this build.
pub const CONTROL_PROTOCOL_VERSION: u16 = 6;

/// Maximum size of one request, reply, or inspector event line, including its
/// trailing newline. The control plane is local and bounded, so oversized
/// input is rejected before JSON parsing can allocate an unbounded value.
pub const CONTROL_MAX_LINE_BYTES: usize = 1024 * 1024;

/// Deadline for one finite request, covering connect, write, and reply body.
pub const CONTROL_REQUEST_TIMEOUT_SECS: u64 = 5;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlRequest {
    pub version: u16,
    #[serde(flatten)]
    pub operation: ControlOperation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum ControlOperation {
    Ready,
    Status,
    Shutdown {
        /// Stop attached frontends for an explicit `omnifs down`. Daemon
        /// replacement leaves them alive so they reconnect.
        #[serde(default)]
        stop_frontends: bool,
    },
    ValidateOffline {
        revision: String,
    },
    SubscribeInspector,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlReply {
    pub version: u16,
    #[serde(flatten)]
    pub outcome: ControlOutcome,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "result", content = "value", rename_all = "snake_case")]
pub enum ControlOutcome {
    Ready,
    Status(DaemonStatus),
    Shutdown {
        detached: usize,
        still_attached: Vec<String>,
    },
    OfflineValidated,
    InspectorReady {
        instance_id: String,
    },
    Error(ControlError),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlError {
    pub code: ControlErrorCode,
    pub message: String,
}

impl ControlError {
    #[must_use]
    pub fn new(code: ControlErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlErrorCode {
    UnsupportedVersion,
    MalformedJson,
    UnknownOperation,
    LineTooLarge,
    NotReady,
    InvalidRequest,
    OfflineValidationFailed,
    Internal,
}

impl ControlReply {
    #[must_use]
    pub fn ready() -> Self {
        Self {
            version: CONTROL_PROTOCOL_VERSION,
            outcome: ControlOutcome::Ready,
        }
    }

    #[must_use]
    pub fn inspector_ready(instance_id: impl Into<String>) -> Self {
        Self {
            version: CONTROL_PROTOCOL_VERSION,
            outcome: ControlOutcome::InspectorReady {
                instance_id: instance_id.into(),
            },
        }
    }

    #[must_use]
    pub fn error(error: ControlError) -> Self {
        Self {
            version: CONTROL_PROTOCOL_VERSION,
            outcome: ControlOutcome::Error(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_and_reply_shapes_are_operation_specific() {
        let request = ControlRequest {
            version: CONTROL_PROTOCOL_VERSION,
            operation: ControlOperation::Shutdown {
                stop_frontends: true,
            },
        };
        assert_eq!(
            serde_json::to_string(&request).unwrap(),
            r#"{"version":6,"operation":"shutdown","stop_frontends":true}"#
        );

        let validate = ControlRequest {
            version: CONTROL_PROTOCOL_VERSION,
            operation: ControlOperation::ValidateOffline {
                revision: "a".repeat(40),
            },
        };
        assert_eq!(
            serde_json::to_string(&validate).unwrap(),
            format!(
                r#"{{"version":6,"operation":"validate_offline","revision":"{}"}}"#,
                "a".repeat(40)
            )
        );

        let reply = ControlReply::error(ControlError::new(
            ControlErrorCode::NotReady,
            "namespace listeners are not serving yet",
        ));
        assert_eq!(
            serde_json::to_string(&reply).unwrap(),
            r#"{"version":6,"result":"error","value":{"code":"not_ready","message":"namespace listeners are not serving yet"}}"#
        );

        assert_eq!(
            serde_json::to_string(&ControlReply::inspector_ready("epoch-1")).unwrap(),
            r#"{"version":6,"result":"inspector_ready","value":{"instance_id":"epoch-1"}}"#
        );

        assert!(
            serde_json::from_value::<ControlRequest>(serde_json::json!({
                "version": CONTROL_PROTOCOL_VERSION,
                "operation": "shutdown"
            }))
            .is_ok()
        );
    }
}
