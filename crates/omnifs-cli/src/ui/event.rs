//! The internal event model renderers consume.
//!
//! No command prints directly; long-running and conversational surfaces emit a
//! stream of [`UiEvent`]s, and a [`Render`] turns that stream into bytes. Three
//! renderers can exist over one stream: the flat [`LedgerRenderer`] built here,
//! a rail/session renderer (a later wave, on cliclack), and an NDJSON renderer
//! for `--progress=json` (a later wave). Keeping the model an enum plus a trait
//! is deliberate: there is no async bus, and progress is driven by the caller.

// This module is the sanctioned output owner; the drift gate denies print
// macros everywhere else.
#![allow(clippy::disallowed_macros, clippy::print_stderr)]

use super::report::Row;
use super::style::{self, Glyph};
use std::time::Duration;

/// One thing that happened, described so any renderer can present it. Narration
/// and progress belong on stderr; the [`LedgerRenderer`] enforces that.
///
// The session (rail) and NDJSON renderers land in later cli-redesign waves and
// construct every variant and read every field; the flat ledger built here
// consumes the subset it needs today.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(crate) enum UiEvent {
    /// Explanatory prose inside a command transcript.
    Narration { message: String },
    /// A titled block began (`Frontends (2)`), rendered as a heading.
    PhaseStarted { title: String, count: Option<usize> },
    /// A row reached its final state: the permanent transcript record.
    RowSettled {
        glyph: Glyph,
        key: String,
        value: String,
        fix: Option<String>,
        duration: Option<Duration>,
    },
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

#[derive(Debug, Clone)]
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

fn format_duration(duration: Duration) -> String {
    if duration.as_secs() > 0 {
        format!("{}s", duration.as_secs())
    } else {
        format!("{}ms", duration.as_millis())
    }
}
