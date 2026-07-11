//! The rail register for commands that may converse with the user.

use cliclack::{Theme, ThemeState};
use std::fmt::Write as _;

use super::consent::{Plan, Receipt};
use super::event::{Render, UiEvent};
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
        let mut out = if symbol.is_empty() {
            format!("│  {first}\n")
        } else {
            format!("{symbol} {first}\n")
        };
        for line in lines {
            let _ = writeln!(out, "│  {line}");
        }
        if !symbol.is_empty() {
            out.push_str("│\n");
        }
        out
    }

    fn format_header(&self, state: &ThemeState, prompt: &str) -> String {
        if matches!(state, ThemeState::Cancel) {
            String::new()
        } else {
            <DefaultTheme as Theme>::format_header(&DefaultTheme, state, prompt)
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
    pub(crate) fn intro(title: impl std::fmt::Display) -> anyhow::Result<Self> {
        cliclack::intro(title)?;
        Ok(Self {
            renderer: RailRenderer,
            closed: false,
        })
    }

    pub(crate) fn phase(&mut self, title: impl Into<String>) {
        self.renderer.event(&UiEvent::PhaseStarted {
            title: title.into(),
            count: None,
        });
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
            fix: row.fix,
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

pub(crate) struct RailRenderer;

impl Render for RailRenderer {
    fn event(&mut self, event: &UiEvent) {
        match event {
            UiEvent::Narration { message } => {
                let _ = cliclack::log::remark(message);
            },
            UiEvent::PhaseStarted { title, .. } => {
                let _ = cliclack::log::step(title);
            },
            UiEvent::Plan {
                rows, remove, keep, ..
            } => {
                let _ = cliclack::log::step("plan");
                for row in rows {
                    let rendered = row.render_plan().render();
                    let _ = cliclack::log::remark(rendered.trim_start());
                }
                let _ = cliclack::log::remark(super::style::dim(format!(
                    "{remove} to remove, {keep} kept"
                )));
            },
            UiEvent::RowSettled {
                glyph, key, value, ..
            } => {
                let row = Row::new(*glyph, key.clone(), value.clone()).render();
                let _ = cliclack::log::remark(row.trim_start());
            },
            UiEvent::Receipt { rows, .. } => {
                let _ = cliclack::log::step("apply");
                for row in rows {
                    let rendered = row.render_receipt().render();
                    let _ = cliclack::log::remark(rendered.trim_start());
                }
            },
            UiEvent::Outro { message } => {
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
        assert_eq!(
            theme.format_log("1/5 environment", "◇"),
            "◇ 1/5 environment\n│\n"
        );
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
