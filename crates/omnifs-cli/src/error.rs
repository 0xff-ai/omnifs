//! Top-level error formatter with structured `Try:` recovery hints.
//!
//! Hints are accumulated on a `HintedError` wrapper that sits at the head of
//! the anyhow error chain. `with_hint` either appends to an existing
//! `HintedError` or creates a new one. The renderer walks the chain,
//! collects hints from the wrapper, and prints them as a `Try:` block beneath
//! the standard "Caused by:" formatting.

use std::borrow::Cow;
use std::fmt::Write as _;

pub(crate) use crate::ui::output::{ErrorEnvelope, ErrorPayload, ErrorVerdict};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExitCode {
    Success,
    GenericFailure,
    /// A clap parse/usage error. Constructed at the `main` parse boundary, never
    /// per command; clap owns the message.
    Usage,
    DaemonUnavailable,
    AuthRequired,
    Degraded,
    /// The operator declined a prompt or pressed Ctrl-C. Mirrors the shell
    /// convention (128 + SIGINT).
    Canceled,
}

impl ExitCode {
    pub(crate) const fn code(self) -> i32 {
        match self {
            Self::Success => 0,
            Self::GenericFailure => 1,
            Self::Usage => 2,
            Self::DaemonUnavailable => 3,
            Self::AuthRequired => 4,
            Self::Degraded => 5,
            Self::Canceled => 130,
        }
    }

    /// Stable, machine-stable slug for this failure class (7.4). It is derived
    /// from the exit class, not the wording, so agents can pattern-match a
    /// failure without scraping prose we are free to reword. The set is
    /// deliberately small and owned here; call sites never invent slugs.
    pub(crate) const fn slug(self) -> &'static str {
        match self {
            Self::Success => "ok",
            Self::GenericFailure => "generic-failure",
            Self::Usage => "usage",
            Self::DaemonUnavailable => "daemon-unavailable",
            Self::AuthRequired => "auth-required",
            Self::Degraded => "degraded",
            Self::Canceled => "canceled",
        }
    }
}

/// Wrapper error that carries `Try:` hints alongside the original cause chain.
///
/// Stored as an `anyhow::Error::new(HintedError { .. })` so that
/// `downcast_ref::<HintedError>()` succeeds on the first element of the chain.
/// Multiple `with_hint` calls append to `hints` rather than stacking wrappers.
#[derive(Debug, thiserror::Error)]
#[error("{source}")]
struct HintedError {
    hints: Vec<Cow<'static, str>>,
    exit_code: ExitCode,
    #[source]
    source: Box<dyn std::error::Error + Send + Sync + 'static>,
}

impl HintedError {
    /// Find the wrapper anywhere in the cause chain so hints and exit codes
    /// survive callers adding context above it.
    fn find(error: &anyhow::Error) -> Option<&Self> {
        error.chain().find_map(|cause| cause.downcast_ref::<Self>())
    }
}

pub trait WithHint<T> {
    fn with_hint(self, hint: impl Into<Cow<'static, str>>) -> anyhow::Result<T>;
}

pub trait WithExitCode<T> {
    fn with_exit_code(self, exit_code: ExitCode) -> anyhow::Result<T>;
}

impl<T, E> WithHint<T> for Result<T, E>
where
    E: Into<anyhow::Error>,
{
    fn with_hint(self, hint: impl Into<Cow<'static, str>>) -> anyhow::Result<T> {
        match self {
            Ok(value) => Ok(value),
            Err(error) => {
                let mut err: anyhow::Error = error.into();
                // If a HintedError is already at the head, downcast and append
                // rather than stacking another wrapper.
                if let Some(hinted) = err.downcast_mut::<HintedError>() {
                    hinted.hints.push(hint.into());
                    return Err(err);
                }
                Err(anyhow::Error::new(HintedError {
                    hints: vec![hint.into()],
                    exit_code: ExitCode::GenericFailure,
                    source: err.into(),
                }))
            },
        }
    }
}

impl<T, E> WithExitCode<T> for Result<T, E>
where
    E: Into<anyhow::Error>,
{
    fn with_exit_code(self, exit_code: ExitCode) -> anyhow::Result<T> {
        match self {
            Ok(value) => Ok(value),
            Err(error) => {
                let mut err: anyhow::Error = error.into();
                if let Some(hinted) = err.downcast_mut::<HintedError>() {
                    hinted.exit_code = exit_code;
                    return Err(err);
                }
                Err(anyhow::Error::new(HintedError {
                    hints: Vec::new(),
                    exit_code,
                    source: err.into(),
                }))
            },
        }
    }
}

pub(crate) fn exit_code(error: &anyhow::Error) -> ExitCode {
    HintedError::find(error).map_or(ExitCode::GenericFailure, |hinted| hinted.exit_code)
}

/// Collect the deduplicated message chain, most-specific first, dropping the
/// empty display strings the `HintedError` wrapper delegates away.
fn message_chain(error: &anyhow::Error) -> Vec<String> {
    error
        .chain()
        .map(ToString::to_string)
        .filter(|s| !s.is_empty())
        .fold(Vec::<String>::new(), |mut messages, message| {
            if messages.last() != Some(&message) {
                messages.push(message);
            }
            messages
        })
}

/// Build the structured terminal envelope without writing to a stream. The
/// command name is supplied by the invocation owner because errors can happen
/// before a command-specific receipt exists.
pub(crate) fn envelope(error: &anyhow::Error, command: impl Into<String>) -> ErrorEnvelope {
    let code = exit_code(error);
    let messages = message_chain(error);
    let hints: Vec<String> = HintedError::find(error)
        .map(|hinted| hinted.hints.iter().map(ToString::to_string).collect())
        .unwrap_or_default();
    let mut messages = messages.into_iter();
    ErrorEnvelope::new(
        command,
        if code == ExitCode::Canceled {
            ErrorVerdict::Canceled
        } else {
            ErrorVerdict::Failed
        },
        ErrorPayload {
            id: code.slug().to_owned(),
            exit_code: code.code(),
            message: messages.next().unwrap_or_default(),
            causes: messages.collect(),
            fix: hints.first().cloned(),
            hints,
        },
    )
}

pub(crate) fn canceled_envelope(
    command: impl Into<String>,
    message: impl Into<String>,
) -> ErrorEnvelope {
    ErrorEnvelope::new(
        command,
        ErrorVerdict::Canceled,
        ErrorPayload {
            id: ExitCode::Canceled.slug().to_owned(),
            exit_code: ExitCode::Canceled.code(),
            message: message.into(),
            causes: Vec::new(),
            fix: None,
            hints: Vec::new(),
        },
    )
}

/// Walks the error chain and renders it as:
///
/// ```text
/// Error: <root message>
///
/// Caused by:
///   <next>
///   <next>
///
/// Try:
///   • <hint>
///   • <hint>
/// ```
pub fn render(error: &anyhow::Error) -> String {
    let mut out = String::new();

    // Collect hints from the HintedError wrapper if present.
    let hints: &[Cow<'static, str>] = HintedError::find(error).map_or(&[], |h| h.hints.as_slice());
    let messages = message_chain(error);

    // Command spans written as `` `cmd` `` render in the cyan accent, never as
    // literal backticks: this is terminal output, not markdown.
    let accent = crate::ui::style::accentuate;
    if let Some(first) = messages.first() {
        let _ = writeln!(&mut out, "Error: {}", accent(first));
    }
    if messages.len() > 1 {
        out.push_str("\nCaused by:\n");
        for msg in &messages[1..] {
            let _ = writeln!(&mut out, "  {}", accent(msg));
        }
    }
    if !hints.is_empty() {
        out.push_str("\nTry:\n");
        for hint in hints {
            let _ = writeln!(&mut out, "  \u{2022} {}", accent(hint));
        }
    }
    // The stable identity, dim, so support and agents can name the failure
    // without matching on wording. Same slug as the JSON `id`.
    let _ = writeln!(
        &mut out,
        "\n{}",
        crate::ui::style::dim(format!("(id: {})", exit_code(error).slug()))
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::strip_ansi;

    #[test]
    fn exit_codes_complete_the_api() {
        assert_eq!(ExitCode::Success.code(), 0);
        assert_eq!(ExitCode::GenericFailure.code(), 1);
        assert_eq!(ExitCode::Usage.code(), 2);
        assert_eq!(ExitCode::DaemonUnavailable.code(), 3);
        assert_eq!(ExitCode::AuthRequired.code(), 4);
        assert_eq!(ExitCode::Degraded.code(), 5);
        assert_eq!(ExitCode::Canceled.code(), 130);
    }

    #[test]
    fn human_block_shows_the_stable_slug() {
        let base = anyhow::anyhow!("boom").context("outer");
        let error = WithExitCode::with_exit_code(
            Err::<(), anyhow::Error>(base),
            ExitCode::DaemonUnavailable,
        )
        .unwrap_err();
        let rendered = strip_ansi(&render(&error));
        assert!(rendered.contains("(id: daemon-unavailable)"), "{rendered}");
    }

    #[test]
    fn structured_error_envelope_keeps_failed_and_canceled_distinct() {
        let base = anyhow::anyhow!("daemon not running");
        let error = WithExitCode::with_exit_code(
            Err::<(), anyhow::Error>(base),
            ExitCode::DaemonUnavailable,
        )
        .unwrap_err();
        let failed = envelope(&error, "status");
        assert_eq!(failed.verdict, ErrorVerdict::Failed);
        assert_eq!(failed.error.exit_code, 3);
        let canceled = canceled_envelope("status", "canceled");
        assert_eq!(canceled.verdict, ErrorVerdict::Canceled);
        assert_eq!(canceled.error.exit_code, 130);
    }

    #[test]
    fn structured_error_json_omits_empty_optional_fields() {
        let envelope = canceled_envelope("status", "canceled");
        let value = serde_json::to_value(envelope).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "schema_version": 1,
                "command": "status",
                "verdict": "canceled",
                "error": {
                    "id": "canceled",
                    "exit_code": 130,
                    "message": "canceled"
                }
            })
        );
    }
}
