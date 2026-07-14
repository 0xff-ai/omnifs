//! Stable JSONL event envelopes.
//!
//! Commands report typed facts to [`super::output::Output`]. This module only
//! owns the public wire shapes for the phase and progress events that Output
//! may serialize before one terminal result or error.

use serde::Serialize;

use super::output::{ErrorEnvelope, ErrorVerdict, ResultVerdict, SCHEMA_VERSION};

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub(crate) enum JsonlEvent {
    Phase(JsonlPhase),
    Progress(JsonlProgress),
}
#[derive(Debug, Clone, Serialize)]
pub(crate) struct JsonlPhase {
    pub(crate) schema_version: u8,
    #[serde(rename = "type")]
    pub(crate) kind: &'static str,
    pub(crate) command: String,
    pub(crate) phase: String,
    pub(crate) state: String,
}

impl JsonlPhase {
    pub(crate) fn new(
        command: impl Into<String>,
        phase: impl Into<String>,
        state: impl Into<String>,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            kind: "phase",
            command: command.into(),
            phase: phase.into(),
            state: state.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct JsonlProgress {
    pub(crate) schema_version: u8,
    #[serde(rename = "type")]
    pub(crate) kind: &'static str,
    pub(crate) command: String,
    pub(crate) resource: String,
    pub(crate) state: String,
    pub(crate) elapsed_ms: u64,
}

impl JsonlProgress {
    pub(crate) fn new(
        command: impl Into<String>,
        resource: impl Into<String>,
        state: impl Into<String>,
        elapsed_ms: u64,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            kind: "progress",
            command: command.into(),
            resource: resource.into(),
            state: state.into(),
            elapsed_ms,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct JsonlResult<T> {
    pub(crate) schema_version: u8,
    #[serde(rename = "type")]
    pub(crate) kind: &'static str,
    pub(crate) command: String,
    pub(crate) verdict: ResultVerdict,
    pub(crate) result: T,
}

impl<T> JsonlResult<T> {
    pub(crate) fn new(command: impl Into<String>, verdict: ResultVerdict, result: T) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            kind: "result",
            command: command.into(),
            verdict,
            result,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct JsonlError {
    pub(crate) schema_version: u8,
    #[serde(rename = "type")]
    pub(crate) kind: &'static str,
    pub(crate) command: String,
    pub(crate) verdict: ErrorVerdict,
    pub(crate) error: super::output::ErrorPayload,
}

impl JsonlError {
    pub(crate) fn from_envelope(envelope: ErrorEnvelope) -> Self {
        Self {
            schema_version: envelope.schema_version,
            kind: "error",
            command: envelope.command,
            verdict: envelope.verdict,
            error: envelope.error,
        }
    }
}
