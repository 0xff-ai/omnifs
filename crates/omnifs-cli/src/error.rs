//! Top-level error formatter with structured `Try:` recovery hints.
//!
//! Hints are accumulated on a `HintedError` wrapper that sits at the head of
//! the anyhow error chain. `with_hint` either appends to an existing
//! `HintedError` or creates a new one. The renderer walks the chain,
//! collects hints from the wrapper, and prints them as a `Try:` block beneath
//! the standard "Caused by:" formatting.

use std::borrow::Cow;
use std::fmt::Write as _;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExitCode {
    Success,
    GenericFailure,
    DaemonUnavailable,
    AuthRequired,
    Degraded,
}

impl ExitCode {
    pub(crate) const fn code(self) -> i32 {
        match self {
            Self::Success => 0,
            Self::GenericFailure => 1,
            Self::DaemonUnavailable => 3,
            Self::AuthRequired => 4,
            Self::Degraded => 5,
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
    error
        .downcast_ref::<HintedError>()
        .map_or(ExitCode::GenericFailure, |hinted| hinted.exit_code)
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
    let hints: &[Cow<'static, str>] = error
        .downcast_ref::<HintedError>()
        .map_or(&[], |h| h.hints.as_slice());

    // Build the message chain from anyhow's chain iterator, skipping empty
    // display strings (the HintedError itself delegates display to its source,
    // but due to the wrapper structure some empty strings may appear).
    let messages = error
        .chain()
        .map(ToString::to_string)
        .filter(|s| !s.is_empty())
        .fold(Vec::<String>::new(), |mut messages, message| {
            if messages.last() != Some(&message) {
                messages.push(message);
            }
            messages
        });

    if let Some(first) = messages.first() {
        let _ = writeln!(&mut out, "Error: {first}");
    }
    if messages.len() > 1 {
        out.push_str("\nCaused by:\n");
        for msg in &messages[1..] {
            let _ = writeln!(&mut out, "  {msg}");
        }
    }
    if !hints.is_empty() {
        out.push_str("\nTry:\n");
        for hint in hints {
            let _ = writeln!(&mut out, "  \u{2022} {hint}");
        }
    }
    out
}
