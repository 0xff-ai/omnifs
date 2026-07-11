//! The internal event model renderers consume.
//!
//! No command prints directly; long-running and conversational surfaces emit a
//! stream of [`UiEvent`]s, and a [`Render`] turns that stream into bytes. Three
//! renderers can exist over one stream: the flat [`LedgerRenderer`] built here,
//! a rail/session renderer (a later wave, on cliclack), and an NDJSON renderer
//! for `--progress=json` (a later wave). Keeping the model an enum plus a trait
//! is deliberate: there is no async bus, and progress is driven by the caller.

// This module is the sanctioned output owner; the drift gate denies print
// macros everywhere else. The NDJSON renderer is the one stdout writer here.
#![allow(clippy::disallowed_macros, clippy::print_stderr, clippy::print_stdout)]

use super::consent::{Outcome, Row as ConsentRow};
use super::report::Row;
use super::style::{self, Glyph};
use serde::Serialize;
use std::time::Duration;

/// One thing that happened, described so any renderer can present it. Narration
/// and progress belong on stderr; the [`LedgerRenderer`] enforces that.
///
// The session (rail) and NDJSON renderers land in later cli-redesign waves and
// construct every variant and read every field; the flat ledger built here
// consumes the subset it needs today.
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum UiEvent {
    /// Explanatory prose inside a command transcript.
    Narration { message: String },
    /// A titled block began (`Frontends (2)`), rendered as a heading.
    PhaseStarted { title: String, count: Option<usize> },
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
    Progress { key: String, message: String },
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
        if super::output::quiet()
            && matches!(
                event,
                UiEvent::Narration { .. } | UiEvent::PromptShown { .. }
            )
        {
            return;
        }
        match event {
            UiEvent::Narration { message } => anstream::eprintln!("{message}"),
            UiEvent::PhaseStarted { title, count } => {
                anstream::eprintln!();
                let heading = match count {
                    Some(count) => format!("{title} ({count})"),
                    None => title.clone(),
                };
                anstream::eprintln!("{}", style::bold(heading));
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

/// The `--progress json` renderer: one serialized [`UiEvent`] per line on
/// stdout. This is the machine face of the same event stream the flat ledger
/// animates, so an agent sees every phase, progress tick, and settle as JSON
/// lines.
///
/// Stream choice: NDJSON goes to stdout (machine output), the same stream a
/// `--json` final receipt uses. A receipt is itself a single-line JSON
/// document, so when both flags are set the combined stdout stays a valid
/// JSON-lines stream whose last line is the receipt.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct NdjsonRenderer;

impl Render for NdjsonRenderer {
    fn event(&mut self, event: &UiEvent) {
        // A UiEvent is always serializable; drop the line rather than panic if
        // that ever changes, so progress never aborts a command.
        if let Ok(line) = serde_json::to_string(event) {
            anstream::println!("{line}");
        }
    }
}

/// The renderer [`super::progress::LiveRow`] uses by default. It dispatches to
/// the flat ledger or the NDJSON stream based on the global `--progress`
/// selection, so every progress-bearing command routes through one type
/// without threading a generic parameter down the call graph.
#[derive(Debug, Clone, Copy)]
pub(crate) enum StreamRenderer {
    Flat(LedgerRenderer),
    Ndjson(NdjsonRenderer),
}

impl StreamRenderer {
    pub(crate) fn from_global() -> Self {
        if super::output::progress_is_json() {
            Self::Ndjson(NdjsonRenderer)
        } else {
            Self::Flat(LedgerRenderer)
        }
    }
}

impl Render for StreamRenderer {
    fn event(&mut self, event: &UiEvent) {
        match self {
            Self::Flat(renderer) => renderer.event(event),
            Self::Ndjson(renderer) => renderer.event(event),
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

    #[test]
    fn ndjson_serializes_one_tagged_object_per_event() {
        // The NDJSON line is a single serialized UiEvent: a `type` tag plus the
        // variant's fields, no trailing newline in the value itself.
        let event = UiEvent::RowSettled {
            glyph: Glyph::Done,
            key: "daemon".to_owned(),
            value: "running".to_owned(),
            fix: None,
            duration: Some(Duration::from_millis(12)),
        };
        let line = serde_json::to_string(&event).unwrap();
        assert!(!line.contains('\n'), "one event is one line: {line}");
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["type"], "row_settled");
        assert_eq!(value["key"], "daemon");
        assert_eq!(value["glyph"], "done");
    }
}
