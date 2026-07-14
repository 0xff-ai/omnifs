//! Shared plan/decision/receipt vocabulary for destructive commands.
//!
//! A consent session has one deliberately boring shape: construct a [`Plan`],
//! render it, resolve a [`Decision`], and render a [`Receipt`] made from the
//! same row identities.  Commands own the side effects; this module owns the
//! invariant that the thing the user approved is the thing the receipt settles.

use std::collections::BTreeMap;

use omnifs_auth::RevokeOutcome;
use serde::Serialize;

use super::event::UiEvent;
use super::output::Output;
use super::report;
use super::style::Glyph;
use crate::stages::PromptMode;

/// One planned operation. `id` is stable for the lifetime of a command and is
/// carried into the settled [`Outcome`]. It is never shown in the human rail,
/// but is part of the event/JSON contract for agents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Row {
    pub(crate) id: String,
    pub(crate) key: String,
    pub(crate) value: String,
    pub(crate) remove: bool,
}

impl Row {
    pub(crate) fn remove(
        id: impl Into<String>,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            key: key.into(),
            value: value.into(),
            remove: true,
        }
    }

    pub(crate) fn keep(
        id: impl Into<String>,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            key: key.into(),
            value: value.into(),
            remove: false,
        }
    }

    pub(crate) fn render_plan(&self) -> report::Row {
        report::Row::new(Glyph::Plan, self.key.clone(), self.value.clone())
    }
}

/// A complete destructive-operation preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Plan {
    pub(crate) title: String,
    pub(crate) rows: Vec<Row>,
}

impl Plan {
    pub(crate) fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            rows: Vec::new(),
        }
    }

    pub(crate) fn push(&mut self, row: Row) {
        self.rows.push(row);
    }

    pub(crate) fn remove_count(&self) -> usize {
        self.rows.iter().filter(|row| row.remove).count()
    }

    pub(crate) fn keep_count(&self) -> usize {
        self.rows.len().saturating_sub(self.remove_count())
    }

    /// Convert the plan into an event. Keeping this on the plan means every
    /// renderer sees the same rows and counts.
    pub(crate) fn event(&self) -> UiEvent {
        UiEvent::Plan {
            title: self.title.clone(),
            rows: self.rows.clone(),
            remove: self.remove_count(),
            keep: self.keep_count(),
        }
    }

    /// Settle outcomes in plan order. An operation that did not produce a
    /// result is represented as a skipped row, rather than silently dropping a
    /// row the user approved.
    pub(crate) fn receipt(&self, outcomes: impl IntoIterator<Item = Outcome>) -> Receipt {
        let outcomes: BTreeMap<String, Outcome> = outcomes
            .into_iter()
            .map(|outcome| (outcome.id.clone(), outcome))
            .collect();
        let rows = self
            .rows
            .iter()
            .map(|planned| {
                outcomes
                    .get(&planned.id)
                    .cloned()
                    .unwrap_or_else(|| Outcome::skip(&planned.id, "not applied"))
                    .with_key(planned.key.clone())
            })
            .collect();
        Receipt {
            title: self.title.clone(),
            rows,
        }
    }
}

/// The result of resolving a destructive-operation question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    /// Apply the plan. `--yes` selects this without invoking a prompt.
    Apply,
    /// `--dry-run` selected a plan-only execution.
    DryRun,
}

impl Decision {
    /// Resolve through the shared [`PromptMode`] policy. In particular, a
    /// non-TTY prints its plan first and then receives the standard actionable
    /// flag hint from `PromptMode::resolve`.
    pub(crate) fn resolve(
        mode: PromptMode,
        dry_run: bool,
        flag_hint: &str,
        output: Output,
    ) -> anyhow::Result<Self> {
        if dry_run {
            return Ok(Self::DryRun);
        }
        let proceed = mode.resolve(
            None,
            || true,
            flag_hint,
            || {
                super::prompt::Confirm::new("Proceed?")
                    .with_default(false)
                    .ask_with_output(output)
            },
        )?;
        Self::from_confirmation(proceed)
    }

    /// A negative confirmation is cancellation, not successful application.
    /// Returning the shared marker keeps exit-code and JSON handling at the
    /// top-level CLI boundary for every consent-driven command.
    fn from_confirmation(proceed: bool) -> anyhow::Result<Self> {
        if proceed {
            Ok(Self::Apply)
        } else {
            Err(super::picker::Canceled.into())
        }
    }
}

/// Severity of one settled operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OutcomeState {
    Done,
    Warn,
    Fail,
    Skip,
}

impl OutcomeState {
    pub(crate) const fn glyph(self) -> Glyph {
        match self {
            Self::Done => Glyph::Done,
            Self::Warn => Glyph::Warn,
            Self::Fail => Glyph::Fail,
            Self::Skip => Glyph::Skip,
        }
    }
}

/// A settled operation. `details` carries typed sub-outcomes such as
/// credential revocation without inventing extra top-level plan rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Outcome {
    pub(crate) id: String,
    /// Human-facing key copied from the plan row that this outcome settles.
    /// Before apply, an outcome may only have its stable operation id; the
    /// plan fills this field when it assembles the receipt.
    pub(crate) key: String,
    pub(crate) state: OutcomeState,
    pub(crate) value: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) details: Vec<Outcome>,
}

impl Outcome {
    pub(crate) fn done(id: impl Into<String>, value: impl Into<String>) -> Self {
        Self::new(id, OutcomeState::Done, value)
    }

    pub(crate) fn warn(id: impl Into<String>, value: impl Into<String>) -> Self {
        Self::new(id, OutcomeState::Warn, value)
    }

    pub(crate) fn fail(id: impl Into<String>, value: impl Into<String>) -> Self {
        Self::new(id, OutcomeState::Fail, value)
    }

    pub(crate) fn skip(id: impl Into<String>, value: impl Into<String>) -> Self {
        Self::new(id, OutcomeState::Skip, value)
    }

    fn new(id: impl Into<String>, state: OutcomeState, value: impl Into<String>) -> Self {
        let id = id.into();
        Self {
            key: id.clone(),
            id,
            state,
            value: value.into(),
            details: Vec::new(),
        }
    }

    /// Set the human-facing key without changing the stable operation id.
    pub(crate) fn with_key(mut self, key: impl Into<String>) -> Self {
        self.key = key.into();
        self
    }

    pub(crate) fn with_detail(mut self, detail: Outcome) -> Self {
        if self.state == OutcomeState::Done && detail.state == OutcomeState::Warn {
            self.state = OutcomeState::Warn;
        }
        if detail.state == OutcomeState::Fail {
            self.state = OutcomeState::Fail;
        }
        self.details.push(detail);
        self
    }

    pub(crate) fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    pub(crate) fn glyph(&self) -> Glyph {
        self.state.glyph()
    }

    pub(crate) fn render_receipt(&self) -> report::Row {
        let value = if self.details.is_empty() {
            self.value.clone()
        } else {
            let details = self
                .details
                .iter()
                .map(|detail| detail.value.as_str())
                .collect::<Vec<_>>()
                .join("; ");
            format!("{}; {details}", self.value)
        };
        report::Row::new(self.glyph(), self.key.clone(), value)
    }

    /// Map the auth service's typed result into the consent receipt vocabulary.
    /// A failed upstream revoke is a warning because the local credential was
    /// still deleted. A local delete failure is a hard failure so callers keep
    /// the mount spec intact.
    pub(crate) fn credential(
        key: &omnifs_workspace::authn::CredentialId,
        outcome: &RevokeOutcome,
    ) -> Self {
        let id = format!("credential:{key}");
        match outcome {
            RevokeOutcome::Revoked | RevokeOutcome::LocalOnly => {
                Self::done(id, outcome.to_string())
            },
            RevokeOutcome::Unsupported | RevokeOutcome::Failed { .. } => {
                Self::warn(id, outcome.to_string())
            },
            RevokeOutcome::DeleteFailed { .. } => Self::fail(id, outcome.to_string()),
        }
    }
}

/// Settled rows for one plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Receipt {
    pub(crate) title: String,
    pub(crate) rows: Vec<Outcome>,
}

impl Receipt {
    pub(crate) fn event(&self) -> UiEvent {
        UiEvent::Receipt {
            title: self.title.clone(),
            rows: self.rows.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_workspace::authn::CredentialId;

    #[test]
    fn plan_counts_and_receipt_keep_stable_ids() {
        let mut plan = Plan::new("reset");
        plan.push(Row::remove("daemon", "daemon", "stop"));
        plan.push(Row::remove("mount:a", "mount a", "delete"));
        plan.push(Row::keep("provider-store", "provider store", "keep"));

        assert_eq!(plan.remove_count(), 2);
        assert_eq!(plan.keep_count(), 1);

        let receipt = plan.receipt([
            Outcome::done("mount:a", "deleted"),
            Outcome::done("daemon", "stopped"),
        ]);
        assert_eq!(
            receipt
                .rows
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            ["daemon", "mount:a", "provider-store"]
        );
        assert_eq!(receipt.rows[2].state, OutcomeState::Skip);
    }

    #[test]
    fn revoke_outcomes_are_typed_for_receipts() {
        let key = CredentialId::new("github", "oauth", "default").unwrap();
        assert_eq!(
            Outcome::credential(&key, &RevokeOutcome::Revoked).state,
            OutcomeState::Done
        );
        assert_eq!(
            Outcome::credential(
                &key,
                &RevokeOutcome::Failed {
                    error: "403".to_owned()
                }
            )
            .state,
            OutcomeState::Warn
        );
        assert_eq!(
            Outcome::credential(
                &key,
                &RevokeOutcome::DeleteFailed {
                    error: "read-only".to_owned()
                }
            )
            .state,
            OutcomeState::Fail
        );
    }

    #[test]
    fn dry_run_decision_never_resolves_a_prompt() {
        let mode = PromptMode {
            interactive: false,
            yes: false,
            no_input: false,
        };
        assert_eq!(
            Decision::resolve(
                mode,
                true,
                "-y",
                Output::new(crate::ui::output::OutputMode::Human, false)
            )
            .unwrap(),
            Decision::DryRun
        );
        let error = Decision::resolve(
            mode,
            false,
            "-y",
            Output::new(crate::ui::output::OutputMode::Human, false),
        )
        .unwrap_err();
        assert!(error.to_string().contains("pass -y or --yes"));
    }

    #[test]
    fn declined_confirmation_is_cancellation() {
        let error = Decision::from_confirmation(false).unwrap_err();
        assert!(super::super::picker::is_canceled(&error));
        assert_eq!(Decision::from_confirmation(true).unwrap(), Decision::Apply);
    }
}
