//! Top-level error formatter with structured `Try:` recovery hints.
//!
//! Hints are accumulated on a `HintedError` wrapper that sits at the head of
//! the anyhow error chain. `with_hint` either appends to an existing
//! `HintedError` or creates a new one. The renderer walks the chain,
//! collects hints from the wrapper, and prints them as a `Try:` block beneath
//! the standard "Caused by:" formatting.

use std::borrow::Cow;
use std::fmt;
use std::fmt::Write as _;

/// Wrapper error that carries `Try:` hints alongside the original cause chain.
///
/// Stored as an `anyhow::Error::new(HintedError { .. })` so that
/// `downcast_ref::<HintedError>()` succeeds on the first element of the chain.
/// Multiple `with_hint` calls append to `hints` rather than stacking wrappers.
#[derive(Debug)]
struct HintedError {
    hints: Vec<Cow<'static, str>>,
    source: Box<dyn std::error::Error + Send + Sync + 'static>,
}

impl fmt::Display for HintedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Delegate to the original cause so the outermost message is correct.
        fmt::Display::fmt(&*self.source, f)
    }
}

impl std::error::Error for HintedError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.source()
    }
}

pub trait WithHint<T> {
    fn with_hint(self, hint: impl Into<Cow<'static, str>>) -> anyhow::Result<T>;
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
                    source: err.into(),
                }))
            },
        }
    }
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
    let messages: Vec<String> = error
        .chain()
        .map(ToString::to_string)
        .filter(|s| !s.is_empty())
        .collect();

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
