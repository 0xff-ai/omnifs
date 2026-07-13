//! The internal event model renderers consume.
//!
//! No command prints directly; long-running and conversational surfaces emit a
//! stream of [`UiEvent`]s, and a [`Render`] turns that stream into bytes. Three
//! renderers consume the same stream: the flat [`LedgerRenderer`], the cliclack
//! rail renderer, and the NDJSON renderer for structured JSONL output. Keeping the
//! model an enum plus a trait is deliberate: there is no async bus, and
//! progress is driven by the caller.

// This module is the sanctioned output owner; the drift gate denies print
// macros everywhere else. The NDJSON renderer is the one stdout writer here.
#![allow(clippy::disallowed_macros, clippy::print_stderr, clippy::print_stdout)]

use super::consent::{Outcome, Row as ConsentRow};
use super::report::Row;
use super::style::{self, Glyph};
use serde::Serialize;
use std::time::Duration;

use super::output::{ErrorEnvelope, ResultVerdict, SCHEMA_VERSION};

/// One thing that happened, described so any renderer can present it. Narration
/// and progress belong on stderr; the [`LedgerRenderer`] enforces that.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum UiEvent {
    /// Explanatory prose inside a command transcript.
    Narration { message: String },
    /// A titled block began, rendered as a heading.
    PhaseStarted { title: String },
    /// A destructive-operation preview. These rows are reused verbatim by the
    /// receipt event, with only their settled glyph/value changing.
    Plan {
        title: String,
        rows: Vec<ConsentRow>,
        remove: usize,
        keep: usize,
    },
    /// A row reached its final state: the permanent transcript record.
    RowSettled {
        glyph: Glyph,
        key: String,
        value: String,
        fix: Option<String>,
        duration: Option<Duration>,
    },
    /// Settled rows for a previously emitted [`UiEvent::Plan`].
    Receipt { title: String, rows: Vec<Outcome> },
    /// A transient progress tick. The flat ledger paints its own in-place
    /// spinner and ignores this; an NDJSON renderer forwards it.
    Progress {
        key: String,
        message: String,
        elapsed_ms: u64,
    },
    /// A prompt was presented to the user.
    PromptShown { question: String },
    /// A prompt was answered; the flat ledger collapses it to one line.
    PromptAnswered {
        question: String,
        answer: PromptAnswer,
    },
    /// The closing hand-off line ("Files are live at ...").
    Outro { message: String },
}

/// Stable JSONL phase/progress envelope. The explicit structs keep
/// `schema_version` first and the event tag second in serialized output.
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
    pub(crate) verdict: super::output::ErrorVerdict,
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

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PromptAnswer {
    Visible(String),
    Secret,
}

/// A sink that turns [`UiEvent`]s into output. Implementors own their stream
/// discipline.
pub(crate) trait Render {
    fn event(&mut self, event: &UiEvent);
}

/// The flat-register renderer: narration, settled rows, and prompt collapses on
/// stderr, matching the flat grid a [`super::report::Report`] draws on stdout.
/// Progress ticks are transient and owned by the live row, so this renderer
/// drops them; the permanent record is the settle line.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct LedgerRenderer;

impl Render for LedgerRenderer {
    fn event(&mut self, event: &UiEvent) {
        // Quiet drops conversational narration and prompt echoes; the record
        // (settled rows, plans, receipts, the outro hand-off) is preserved.
        match event {
            UiEvent::Narration { message } => anstream::eprintln!("{message}"),
            UiEvent::PhaseStarted { title } => {
                anstream::eprintln!();
                anstream::eprintln!("{}", style::bold(title));
            },
            UiEvent::Plan {
                title,
                rows,
                remove,
                keep,
            } => {
                anstream::eprintln!();
                anstream::eprintln!("{}", style::bold(title));
                for row in rows {
                    anstream::eprintln!("{}", row.render_plan().render());
                }
                anstream::eprintln!("{}", style::dim(format!("{remove} to remove, {keep} kept")));
            },
            UiEvent::RowSettled {
                glyph,
                key,
                value,
                duration,
                ..
            } => {
                let value = match duration {
                    Some(duration) => format!(
                        "{value} {}",
                        style::dim(format!("({})", format_duration(*duration)))
                    ),
                    None => value.clone(),
                };
                let row = Row::new(*glyph, key.clone(), value);
                anstream::eprintln!("{}", row.render());
            },
            UiEvent::Receipt { title, rows } => {
                anstream::eprintln!();
                anstream::eprintln!("{}", style::bold(title));
                for row in rows {
                    anstream::eprintln!("{}", row.render_receipt().render());
                }
            },
            UiEvent::Progress { .. } | UiEvent::PromptShown { .. } => {},
            UiEvent::PromptAnswered { question, answer } => {
                let answer = match answer {
                    PromptAnswer::Visible(answer) => style::accent(answer),
                    PromptAnswer::Secret => style::dim("answered"),
                };
                anstream::eprintln!("{} {question} {}", Glyph::Done.render(), answer);
            },
            UiEvent::Outro { message } => {
                anstream::eprintln!();
                anstream::eprintln!("{message}");
            },
        }
    }
}

/// The JSONL renderer: one serialized public [`JsonlEvent`] per line on stdout.
/// It exposes the phase and progress subset of the internal event stream, while
/// terminal results and errors retain their own stable envelopes.
///
/// Stream choice: NDJSON goes to stdout (machine output), the same stream a
/// structured final receipt uses. A receipt is itself a single-line JSON
/// document, so when both flags are set the combined stdout stays a valid
/// JSON-lines stream whose last line is the receipt.
#[derive(Debug, Clone, Copy)]
pub(crate) struct NdjsonRenderer {
    output: super::output::Output,
}

impl Render for NdjsonRenderer {
    fn event(&mut self, event: &UiEvent) {
        let event = match event {
            UiEvent::PhaseStarted { title } => JsonlEvent::Phase(JsonlPhase::new(
                self.output.command(),
                title.clone(),
                "started",
            )),
            UiEvent::Progress {
                key,
                message,
                elapsed_ms,
            } => JsonlEvent::Progress(JsonlProgress::new(
                self.output.command(),
                key.clone(),
                message.clone(),
                *elapsed_ms,
            )),
            UiEvent::Narration { .. }
            | UiEvent::Plan { .. }
            | UiEvent::RowSettled { .. }
            | UiEvent::Receipt { .. }
            | UiEvent::PromptShown { .. }
            | UiEvent::PromptAnswered { .. }
            | UiEvent::Outro { .. } => return,
        };
        let mut stdout = std::io::stdout().lock();
        let _ = self.output.write_event(&mut stdout, &event);
    }
}

/// The renderer [`super::progress::LiveRow`] uses by default. It dispatches to
/// the flat ledger or the NDJSON stream based on the invocation output policy,
/// so every progress-bearing command routes through one type.
#[derive(Debug, Clone, Copy)]
pub(crate) enum StreamRenderer {
    Flat(LedgerRenderer),
    Ndjson(NdjsonRenderer),
    Silent,
}

impl StreamRenderer {
    pub(crate) fn from_output(output: super::output::Output) -> Self {
        match output.mode() {
            super::output::OutputMode::Human => Self::Flat(LedgerRenderer),
            super::output::OutputMode::Jsonl => Self::Ndjson(NdjsonRenderer { output }),
            super::output::OutputMode::Json => Self::Silent,
        }
    }
}

impl Render for StreamRenderer {
    fn event(&mut self, event: &UiEvent) {
        match self {
            Self::Flat(renderer) => renderer.event(event),
            Self::Ndjson(renderer) => renderer.event(event),
            Self::Silent => {},
        }
    }
}

fn format_duration(duration: Duration) -> String {
    if duration.as_secs() > 0 {
        format!("{}s", duration.as_secs())
    } else {
        format!("{}ms", duration.as_millis())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::output::{ErrorEnvelope, ErrorPayload, ErrorVerdict, Output, ResultVerdict};

    #[test]
    fn jsonl_event_serializes_one_stable_tagged_object_per_event() {
        // Public NDJSON carries only the documented phase/progress envelopes,
        // never the internal UiEvent shape.
        let event = JsonlEvent::Progress(JsonlProgress::new("status", "daemon", "starting", 12));
        let line = serde_json::to_string(&event).unwrap();
        assert!(!line.contains('\n'), "one event is one line: {line}");
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["type"], "progress");
        assert_eq!(value["resource"], "daemon");
        assert_eq!(value["elapsed_ms"], 12);
    }

    #[test]
    fn jsonl_result_and_error_are_terminal_and_distinct() {
        let result =
            Output::jsonl_result_bytes("up", ResultVerdict::Ok, serde_json::json!({})).unwrap();
        let error = Output::jsonl_error_bytes(ErrorEnvelope::new(
            "up",
            ErrorVerdict::Canceled,
            ErrorPayload {
                id: "canceled".into(),
                exit_code: 130,
                message: "canceled".into(),
                causes: Vec::new(),
                fix: None,
                hints: Vec::new(),
            },
        ))
        .unwrap();
        let result: serde_json::Value = serde_json::from_slice(&result).unwrap();
        let error: serde_json::Value = serde_json::from_slice(&error).unwrap();
        assert_eq!(result["type"], "result");
        assert_eq!(result["verdict"], "ok");
        assert_eq!(error["type"], "error");
        assert_eq!(error["verdict"], "canceled");
        assert_ne!(result["type"], error["type"]);
    }
}
