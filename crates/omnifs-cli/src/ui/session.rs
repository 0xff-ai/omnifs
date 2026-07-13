//! The rail register for commands that may converse with the user.

use cliclack::{Theme, ThemeState};
use std::fmt::Write as _;

use super::consent::{Outcome, Plan, Receipt, Row as ConsentRow};
use super::event::{Render, UiEvent};
use super::output::Output;
use super::report::Row;

struct DefaultTheme;
impl Theme for DefaultTheme {}

struct OmnifsTheme;

impl Theme for OmnifsTheme {
    fn format_intro(&self, title: &str) -> String {
        format!("┌ {title}\n│\n")
    }

    fn format_outro(&self, message: &str) -> String {
        format!("└ {message}\n")
    }

    fn remark_symbol(&self) -> String {
        String::new()
    }

    fn format_log(&self, text: &str, symbol: &str) -> String {
        let mut lines = text.lines();
        let Some(first) = lines.next() else {
            return "│\n".to_string();
        };
        // A symboled line (a `◇` phase marker) opens a new section, so lead it
        // with a blank spine to separate it from the rows above. Remark rows
        // (empty symbol) carry no blank, so rows within a group pack tightly.
        let mut out = if symbol.is_empty() {
            format!("│  {first}\n")
        } else {
            format!("│\n{symbol} {}\n", super::style::heading(first))
        };
        for line in lines {
            let _ = writeln!(out, "│  {line}");
        }
        out
    }

    fn format_header(&self, state: &ThemeState, prompt: &str) -> String {
        if matches!(state, ThemeState::Cancel) {
            String::new()
        } else {
            // Lead every prompt with a blank spine so a `◆` question separates
            // from the rows above it (remark rows pack tightly and carry no
            // trailing spine of their own).
            format!(
                "│\n{}",
                <DefaultTheme as Theme>::format_header(&DefaultTheme, state, prompt)
            )
        }
    }

    fn format_footer(&self, state: &ThemeState) -> String {
        if matches!(state, ThemeState::Cancel) {
            String::new()
        } else {
            <DefaultTheme as Theme>::format_footer(&DefaultTheme, state)
        }
    }

    fn format_input(&self, state: &ThemeState, cursor: &cliclack::StringCursor) -> String {
        if matches!(state, ThemeState::Cancel) {
            String::new()
        } else {
            <DefaultTheme as Theme>::format_input(&DefaultTheme, state, cursor)
        }
    }

    fn format_placeholder(&self, state: &ThemeState, cursor: &cliclack::StringCursor) -> String {
        if matches!(state, ThemeState::Cancel) {
            String::new()
        } else {
            <DefaultTheme as Theme>::format_placeholder(&DefaultTheme, state, cursor)
        }
    }

    fn format_select_item(
        &self,
        state: &ThemeState,
        selected: bool,
        label: &str,
        hint: &str,
    ) -> String {
        if matches!(state, ThemeState::Cancel) {
            String::new()
        } else {
            <DefaultTheme as Theme>::format_select_item(&DefaultTheme, state, selected, label, hint)
        }
    }

    fn format_confirm(&self, state: &ThemeState, confirm: bool) -> String {
        if matches!(state, ThemeState::Cancel) {
            String::new()
        } else {
            <DefaultTheme as Theme>::format_confirm(&DefaultTheme, state, confirm)
        }
    }
}

pub(crate) fn install_theme() {
    cliclack::set_theme(OmnifsTheme);
}

pub(crate) struct Session {
    renderer: RailRenderer,
    closed: bool,
}

impl Session {
    /// Construct a conversational session only after the invocation policy
    /// allows interactive output. This keeps structured and no-input modes
    /// from rendering even the intro rail before failing.
    pub(crate) fn intro_with_output(
        title: impl std::fmt::Display,
        output: Output,
    ) -> anyhow::Result<Self> {
        if output.is_structured() {
            return Ok(Self {
                renderer: RailRenderer::new(output),
                closed: false,
            });
        }
        cliclack::intro(title)?;
        Ok(Self {
            renderer: RailRenderer::new(output),
            closed: false,
        })
    }

    pub(crate) fn phase(&mut self, title: impl Into<String>) {
        self.renderer.event(&UiEvent::PhaseStarted {
            title: title.into(),
        });
    }

    pub(crate) const fn output(&self) -> Output {
        self.renderer.output
    }

    pub(crate) fn progress(&self, length: u64, message: impl std::fmt::Display) -> SessionProgress {
        SessionProgress::new(self.output(), length, message)
    }

    /// Emit one destructive-operation preview. The same [`Plan`] is later
    /// passed to [`Self::receipt`], preserving row identity across the rail.
    pub(crate) fn plan(&mut self, plan: &Plan) {
        self.renderer.event(&plan.event());
    }

    /// Emit the settled receipt for a previously displayed plan.
    pub(crate) fn receipt(&mut self, receipt: &Receipt) {
        self.renderer.event(&receipt.event());
    }

    pub(crate) fn row(&mut self, row: Row) {
        self.renderer.event(&UiEvent::RowSettled {
            glyph: row.glyph,
            key: row.key,
            value: row.value,
            fix: None,
            duration: None,
        });
    }

    pub(crate) fn note(&mut self, message: impl std::fmt::Display) {
        self.renderer.event(&UiEvent::Narration {
            message: message.to_string(),
        });
    }

    pub(crate) fn outro(&mut self, message: impl Into<String>) {
        if self.closed {
            return;
        }
        self.renderer.event(&UiEvent::Outro {
            message: message.into(),
        });
        self.closed = true;
    }
}

/// A Cliclack progress bar owned by a conversational session. Structured and
/// quiet output keep the same no-human-output policy as the rest of the UI.
pub(crate) struct SessionProgress {
    bar: Option<cliclack::ProgressBar>,
}

impl SessionProgress {
    fn new(output: Output, length: u64, message: impl std::fmt::Display) -> Self {
        let bar = (!output.is_structured() && !output.quiet()).then(|| {
            let bar = if length == 0 {
                cliclack::spinner()
            } else {
                cliclack::progress_bar(length)
            };
            bar.start(message);
            bar
        });
        Self { bar }
    }

    pub(crate) fn set_message(&self, message: impl std::fmt::Display) {
        if let Some(bar) = &self.bar {
            bar.set_message(message);
        }
    }

    pub(crate) fn inc(&self, delta: u64) {
        if let Some(bar) = &self.bar {
            bar.inc(delta);
        }
    }

    pub(crate) fn stop(&mut self, message: impl std::fmt::Display) {
        if let Some(bar) = self.bar.take() {
            bar.stop(message);
        }
    }

    pub(crate) fn error(&mut self, message: impl std::fmt::Display) {
        if let Some(bar) = self.bar.take() {
            bar.error(message);
        }
    }
}

impl Drop for SessionProgress {
    fn drop(&mut self) {
        if let Some(bar) = self.bar.take() {
            bar.clear();
        }
    }
}

pub(crate) struct RailRenderer {
    output: Output,
}

impl RailRenderer {
    pub(crate) fn new(output: Output) -> Self {
        Self { output }
    }
}

impl Render for RailRenderer {
    fn event(&mut self, event: &UiEvent) {
        match event {
            UiEvent::Narration { message } => {
                if !self.output.quiet() && !self.output.is_structured() {
                    let _ = cliclack::log::remark(message);
                }
            },
            UiEvent::PhaseStarted { title } => {
                if self.output.is_structured() {
                    if self.output.mode() == super::output::OutputMode::Jsonl {
                        let event = super::event::JsonlEvent::Phase(super::event::JsonlPhase::new(
                            self.output.command(),
                            title.clone(),
                            "started",
                        ));
                        let mut stdout = std::io::stdout().lock();
                        let _ = self.output.write_event(&mut stdout, &event);
                    }
                    return;
                }
                let _ = cliclack::log::step(title);
            },
            UiEvent::Plan {
                rows, remove, keep, ..
            } => {
                if self.output.is_structured() {
                    return;
                }
                let _ = cliclack::log::step("plan");
                let rendered = rows.iter().map(ConsentRow::render_plan).collect::<Vec<_>>();
                let _ = cliclack::log::remark(super::report::render_rows(&rendered));
                let _ = cliclack::log::remark(super::style::dim(format!(
                    "{remove} to remove, {keep} kept"
                )));
            },
            UiEvent::RowSettled {
                glyph, key, value, ..
            } => {
                if self.output.is_structured() {
                    return;
                }
                let row = Row::new(*glyph, key.clone(), value.clone()).render();
                let _ = cliclack::log::remark(row.trim_start());
            },
            UiEvent::Receipt { rows, .. } => {
                if self.output.is_structured() {
                    return;
                }
                let _ = cliclack::log::step("apply");
                let rendered = rows.iter().map(Outcome::render_receipt).collect::<Vec<_>>();
                let _ = cliclack::log::remark(super::report::render_rows(&rendered));
            },
            UiEvent::Outro { message } => {
                if self.output.is_structured() {
                    return;
                }
                let _ = cliclack::outro(message);
            },
            UiEvent::Progress { .. }
            | UiEvent::PromptShown { .. }
            | UiEvent::PromptAnswered { .. } => {},
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_renders_the_session_rail() {
        let theme = OmnifsTheme;
        assert_eq!(theme.format_intro("omnifs setup"), "┌ omnifs setup\n│\n");
        // A phase marker opens a section: lead with a blank spine, bold the
        // title, and carry no trailing spine so the section's rows pack under it.
        let step = crate::ui::strip_ansi(&theme.format_log("1/5 environment", "◇"));
        assert_eq!(step, "│\n◇ 1/5 environment\n");
        // Remark rows pack tightly: no leading or trailing spine.
        assert_eq!(
            theme.format_log("✓ daemon  ready", ""),
            "│  ✓ daemon  ready\n"
        );
        assert_eq!(theme.format_outro("You're set."), "└ You're set.\n");
    }

    #[test]
    fn cancel_state_is_silent() {
        let theme = OmnifsTheme;
        assert!(
            theme
                .format_header(&ThemeState::Cancel, "Question?")
                .is_empty()
        );
        assert!(theme.format_footer(&ThemeState::Cancel).is_empty());
    }
}
