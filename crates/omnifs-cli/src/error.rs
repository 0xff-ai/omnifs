//! Top-level error formatter with structured `Try:` recovery hints.
//!
//! Hints are accumulated on a `HintedError` wrapper that sits at the head of
//! the anyhow error chain. `with_hint` either appends to an existing
//! `HintedError` or creates a new one. The human renderer walks
//! the chain, collects hints from the wrapper, and turns them into the
//! `render.rs` error block: a headline, an optional detail (a daemon log
//! tail when the failure is daemon-shaped, otherwise the cause chain), and
//! `Fix:`/`Log:`/`Try:` action lines.

use std::borrow::Cow;

use crate::ui::render;

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

/// The tail of the daemon log quoted inline under a daemon-shaped failure
/// plus the display path used for the accompanying `Log:`
/// action. Read once at the top-level error boundary; never constructed
/// speculatively for a non-daemon failure.
struct DaemonLogTail {
    lines: Vec<String>,
    display_path: String,
}

/// Filter a daemon log to the final error and its immediate context, capped
/// at 5 lines: the quoted block is a diagnosis, not a dump. Pure
/// so the filtering itself is testable without a real log file.
const DAEMON_LOG_TAIL_MAX_LINES: usize = 5;

fn tail_log_lines(contents: &str) -> Vec<String> {
    let lines: Vec<&str> = contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if lines.is_empty() {
        return Vec::new();
    }
    let start = lines
        .iter()
        .rposition(|line| line.contains("ERROR"))
        .unwrap_or_else(|| lines.len().saturating_sub(DAEMON_LOG_TAIL_MAX_LINES));
    let end = lines.len().min(start + DAEMON_LOG_TAIL_MAX_LINES);
    lines[start..end]
        .iter()
        .map(|line| (*line).to_owned())
        .collect()
}

/// Best-effort read of the current workspace's daemon log tail. Any I/O or
/// workspace-resolution failure degrades to `None`.
fn read_daemon_log_tail() -> Option<DaemonLogTail> {
    let workspace = omnifs_workspace::Workspace::resolve().ok()?;
    let log_path = workspace.daemon().log_file();
    let contents = std::fs::read_to_string(&log_path).ok()?;
    let lines = tail_log_lines(&contents);
    if lines.is_empty() {
        return None;
    }
    Some(DaemonLogTail {
        lines,
        display_path: omnifs_workspace::display(&log_path),
    })
}

/// Assemble the human error block from an error chain and an
/// optional daemon log tail. Pure: the caller decides whether the failure is
/// daemon-shaped and does the (real or injected) log read, so this function
/// stays testable without touching a filesystem.
fn build_error_block(
    error: &anyhow::Error,
    daemon_log: Option<&DaemonLogTail>,
) -> render::ErrorBlock {
    let code = exit_code(error);
    let messages = message_chain(error);
    let hints: Vec<String> = HintedError::find(error)
        .map(|hinted| hinted.hints.iter().map(ToString::to_string).collect())
        .unwrap_or_default();

    let headline = messages
        .first()
        .cloned()
        .unwrap_or_else(|| "omnifs failed.".to_owned());

    let detail = if let Some(tail) = daemon_log {
        Some(render::ErrorDetail {
            heading: "Last daemon log lines:".to_owned(),
            lines: tail.lines.clone(),
        })
    } else if messages.len() > 1 {
        Some(render::ErrorDetail {
            heading: "Caused by:".to_owned(),
            lines: messages[1..].to_vec(),
        })
    } else {
        None
    };

    let mut actions = Vec::new();
    let mut hints_iter = hints.into_iter();
    if let Some(fix) = hints_iter.next() {
        actions.push(render::ErrorAction::fix(fix));
    }
    if let Some(tail) = daemon_log {
        actions.push(render::ErrorAction::log(tail.display_path.clone()));
    }
    for hint in hints_iter {
        actions.push(render::ErrorAction::try_(hint));
    }

    render::ErrorBlock {
        headline,
        detail,
        actions,
        id: Some(code.slug().to_owned()),
    }
}

/// Renders the top-level human error block. A daemon-shaped
/// failure (`ExitCode::DaemonUnavailable`) quotes the daemon log tail inline
/// instead of only pointing at `omnifs logs`; every other failure falls back
/// to the plain cause chain.
pub fn render(error: &anyhow::Error) -> String {
    let daemon_log = (exit_code(error) == ExitCode::DaemonUnavailable)
        .then(read_daemon_log_tail)
        .flatten();
    let block = build_error_block(error, daemon_log.as_ref());
    let caps = crate::ui::output::stderr_capabilities(false);
    render::error_block(&block, caps)
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

    fn daemon_unreachable_error(message: &str, fix: &str) -> anyhow::Error {
        WithHint::with_hint(
            WithExitCode::with_exit_code(
                Err::<(), anyhow::Error>(anyhow::anyhow!(message.to_owned())),
                ExitCode::DaemonUnavailable,
            ),
            fix.to_owned(),
        )
        .unwrap_err()
    }

    #[test]
    fn human_error_block_matches_the_documented_shape_with_a_daemon_log_tail() {
        // the worked example, exercised through error.rs's own
        // construction (not just render.rs's primitive test) to prove the
        // wiring: headline, `Last daemon log lines:` detail, `Fix:`/`Log:`.
        let error = daemon_unreachable_error(
            "The daemon exited before your mounts came ready.",
            "omnifs mount add github",
        );
        let tail = DaemonLogTail {
            lines: vec!["ERROR provider github: pinned artifact missing from store".to_owned()],
            display_path: "~/.omnifs/cache/daemon.log".to_owned(),
        };
        let block = build_error_block(&error, Some(&tail));
        let caps = crate::ui::render::Capabilities {
            width: 120,
            is_tty: false,
            color: false,
            quiet: false,
        };
        let rendered = crate::ui::render::error_block(&block, caps);
        assert_eq!(
            rendered,
            "✗ The daemon exited before your mounts came ready.\n\
             \n\
             \x20\x20Last daemon log lines:\n\
             \x20\x20\x20\x20ERROR provider github: pinned artifact missing from store\n\
             \n\
             Fix:  omnifs mount add github\n\
             Log:  ~/.omnifs/cache/daemon.log\n\
             \n\
             (id: daemon-unavailable)\n"
        );
    }

    #[test]
    fn human_error_block_falls_back_to_the_cause_chain_without_a_daemon_log() {
        let error = anyhow::anyhow!("boom").context("outer");
        let block = build_error_block(&error, None);
        assert_eq!(block.headline, "outer");
        let detail = block.detail.expect("cause chain becomes the detail");
        assert_eq!(detail.heading, "Caused by:");
        assert_eq!(detail.lines, vec!["boom".to_owned()]);
    }

    #[test]
    fn human_error_block_never_duplicates_a_nested_id_trailer() {
        // A cause that already carries a rendered `(id: ...)` trailer (e.g. a
        // pre-rendered nested error folded into the message chain) must not
        // duplicate it once the outer block adds its own trailer.
        let error =
            anyhow::anyhow!("upstream failed (id: mount-degraded)").context("Mount degraded.");
        let block = build_error_block(&error, None);
        let caps = crate::ui::render::Capabilities {
            width: 120,
            is_tty: false,
            color: false,
            quiet: false,
        };
        let rendered = crate::ui::render::error_block(&block, caps);
        assert_eq!(rendered.matches("(id: ").count(), 1, "{rendered}");
    }

    #[test]
    fn tail_log_lines_finds_the_final_error_and_caps_at_five_lines() {
        let contents = (0..10)
            .map(|i| format!("INFO line {i}"))
            .chain(std::iter::once("ERROR boom".to_owned()))
            .chain((0..10).map(|i| format!("INFO after {i}")))
            .collect::<Vec<_>>()
            .join("\n");
        let tail = tail_log_lines(&contents);
        assert_eq!(tail.len(), 5);
        assert_eq!(tail[0], "ERROR boom");

        let no_error = (0..20)
            .map(|i| format!("INFO line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let tail = tail_log_lines(&no_error);
        assert_eq!(tail.len(), 5);
        assert_eq!(tail[4], "INFO line 19");

        assert!(tail_log_lines("").is_empty());
        assert!(tail_log_lines("\n\n  \n").is_empty());
    }
}
