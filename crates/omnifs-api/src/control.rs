//! Typed local control-plane wire types.

use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;
use std::path::PathBuf;

use crate::DaemonStatus;

/// The only control protocol version understood by this build.
pub const CONTROL_PROTOCOL_VERSION: u16 = 1;

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
    Shutdown,
    AttachTcp {
        #[serde(default)]
        bind_ip: Option<Ipv4Addr>,
    },
    AttachVsock,
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
    Shutdown,
    AttachTcp(TcpAttachTarget),
    AttachVsock(VsockAttachTarget),
    InspectorReady,
    Error(ControlError),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TcpAttachTarget {
    pub addr: String,
    pub token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VsockAttachTarget {
    pub socket_path: PathBuf,
    pub token: String,
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
    pub fn inspector_ready() -> Self {
        Self {
            version: CONTROL_PROTOCOL_VERSION,
            outcome: ControlOutcome::InspectorReady,
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
            operation: ControlOperation::AttachTcp {
                bind_ip: Some(Ipv4Addr::LOCALHOST),
            },
        };
        assert_eq!(
            serde_json::to_string(&request).unwrap(),
            r#"{"version":1,"operation":"attach_tcp","bind_ip":"127.0.0.1"}"#
        );

        let reply = ControlReply::error(ControlError::new(
            ControlErrorCode::NotReady,
            "namespace listeners are not serving yet",
        ));
        assert_eq!(
            serde_json::to_string(&reply).unwrap(),
            r#"{"version":1,"result":"error","value":{"code":"not_ready","message":"namespace listeners are not serving yet"}}"#
        );

        assert!(
            serde_json::from_value::<TcpAttachTarget>(serde_json::json!({
                "addr": "127.0.0.1:1234",
                "token": "secret",
                "unexpected": true
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<ControlRequest>(serde_json::json!({
                "version": CONTROL_PROTOCOL_VERSION,
                "operation": "attach_tcp"
            }))
            .is_ok()
        );
    }
}
