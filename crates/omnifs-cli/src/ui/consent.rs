//! Shared plan/decision/receipt vocabulary for destructive commands.
//!
//! A consent flow has one deliberately boring shape: construct a [`Plan`],
//! render it, resolve a [`Decision`], and render a [`Receipt`] made from the
//! same row identities.  Commands own the side effects; this module owns the
//! invariant that the thing the user approved is the thing the receipt settles.

use std::collections::BTreeMap;

use serde::Serialize;

use super::output::Output;
use super::render::LedgerRow;
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

    /// Render as one ledger row: `-` for a planned removal, `=` for a planned
    /// keep (spec 2.7's glyph vocabulary). This is the only consumer of
    /// [`Glyph::Keep`]; a plan row that survives is stated, never omitted.
    pub(crate) fn ledger_row(&self) -> LedgerRow {
        let glyph = if self.remove {
            Glyph::Plan
        } else {
            Glyph::Keep
        };
        LedgerRow::new(glyph, self.key.clone(), self.value.clone())
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

    /// Past-tense counts for the closing sentence once a plan has settled
    /// (spec 2.7: `Removed github. 2 removed, 1 kept.`). Kept rows are stated
    /// here rather than in the plan preview, since a plan preview already
    /// marks each row `-`/`=` individually.
    pub(crate) fn settled_summary(&self) -> String {
        format!(
            "{} removed, {} kept",
            self.remove_count(),
            self.keep_count()
        )
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
    /// flag hint from `PromptMode::resolve`. `question` is the confirm
    /// prompt's verb (`"Remove?"`), so the transcript names the actual
    /// operation rather than a generic "Proceed?" (spec 3.5).
    pub(crate) fn resolve(
        mode: PromptMode,
        dry_run: bool,
        question: &str,
        flag_hint: &str,
        output: &Output,
    ) -> anyhow::Result<Self> {
        if dry_run {
            return Ok(Self::DryRun);
        }
        let proceed = mode.resolve(
            None,
            || true,
            flag_hint,
            || {
                super::prompt::Confirm::new(question)
                    .with_default(false)
                    .ask_with_output(output)
            },
        )?;
        Self::from_confirmation(proceed, output)
    }

    /// A negative confirmation is cancellation, not successful application.
    /// Returning the shared marker keeps exit-code and JSON handling at the
    /// top-level CLI boundary for every consent-driven command. An interactive
    /// decline prints the closing line itself (spec 2.7: `Kept everything as
    /// it was.`) since the caller never regains control after the `?`
    /// propagates the cancellation.
    fn from_confirmation(proceed: bool, output: &Output) -> anyhow::Result<Self> {
        if proceed {
            Ok(Self::Apply)
        } else {
            output.outro("Kept everything as it was.");
            Err(super::prompt::Canceled.into())
        }
    }
}

/// Severity of one settled operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OutcomeState {
    Done,
    Fail,
    Skip,
}

impl OutcomeState {
    pub(crate) const fn glyph(self) -> Glyph {
        match self {
            Self::Done => Glyph::Done,
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

    pub(crate) fn glyph(&self) -> Glyph {
        self.state.glyph()
    }

    /// The value column, with any sub-outcomes (`details`) folded in as a
    /// `;`-joined trailer. Shared by both receipt renderers so a credential
    /// revocation's sub-outcome never has to be reformatted twice.
    fn settled_value(&self) -> String {
        if self.details.is_empty() {
            return self.value.clone();
        }
        let details = self
            .details
            .iter()
            .map(|detail| detail.value.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        format!("{}; {details}", self.value)
    }

    /// Render as one v2-register ledger row (spec 2.1/2.7): `✓`/`✗`/`•` per
    /// [`OutcomeState`], never `-`/`=` (those are plan-preview-only glyphs).
    pub(crate) fn ledger_row(&self) -> LedgerRow {
        LedgerRow::new(self.glyph(), self.key.clone(), self.settled_value())
    }
}

/// Settled rows for one plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Receipt {
    pub(crate) title: String,
    pub(crate) rows: Vec<Outcome>,
}

#[cfg(test)]
mod tests {
    use super::*;

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
                "Remove?",
                "-y",
                &Output::new(crate::ui::output::OutputMode::Human, false)
            )
            .unwrap(),
            Decision::DryRun
        );
        let error = Decision::resolve(
            mode,
            false,
            "Remove?",
            "-y",
            &Output::new(crate::ui::output::OutputMode::Human, false),
        )
        .unwrap_err();
        assert!(error.to_string().contains("pass -y or --yes"));
    }

    #[test]
    fn declined_confirmation_is_cancellation() {
        let output = Output::new(crate::ui::output::OutputMode::Human, false);
        let error = Decision::from_confirmation(false, &output).unwrap_err();
        assert!(super::super::prompt::is_canceled(&error));
        assert_eq!(
            Decision::from_confirmation(true, &output).unwrap(),
            Decision::Apply
        );
    }

    #[test]
    fn ledger_row_uses_the_minus_glyph_for_removal_and_equals_for_keep() {
        let remove = Row::remove("mount", "mount", "/github").ledger_row();
        assert_eq!(remove.glyph, Glyph::Plan);
        let keep = Row::keep("provider", "provider", "artifact kept in store").ledger_row();
        assert_eq!(keep.glyph, Glyph::Keep);
    }

    #[test]
    fn settled_summary_reports_removed_and_kept_counts() {
        let mut plan = Plan::new("Removing mount `github`");
        plan.push(Row::remove("mount", "mount", "/github"));
        plan.push(Row::remove("credential", "credential", "github oauth"));
        plan.push(Row::keep("provider", "provider", "artifact kept in store"));
        assert_eq!(plan.settled_summary(), "2 removed, 1 kept");
    }

    #[test]
    fn receipt_ledger_row_folds_details_into_the_value_trailer() {
        let mut outcome = Outcome::done("credential", "revoked").with_key("credential");
        outcome
            .details
            .push(Outcome::done("upstream", "revoked upstream"));
        let row = outcome.ledger_row();
        assert_eq!(row.value, "revoked; revoked upstream");
        assert_eq!(row.glyph, Glyph::Done);
    }
}
