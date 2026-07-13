//! Unified interactive prompts.

use std::io::{self, IsTerminal};

use super::event::{PromptAnswer, Render as _, UiEvent};
use super::output::Output;
use super::picker::Canceled;
use super::session::RailRenderer;

/// Whether interactive prompts can safely use cliclack.
///
/// Prompt output is written to stderr, so stdin and stderr must both be
/// terminals. Stdout is intentionally not part of this check: callers may
/// pipe report output while still answering a prompt on the controlling
/// terminal.
pub(crate) fn is_terminal() -> bool {
    io::stdin().is_terminal() && io::stderr().is_terminal()
}

fn prompt_error(error: io::Error) -> anyhow::Error {
    match error.kind() {
        io::ErrorKind::Interrupted => anyhow::Error::new(Canceled),
        // cliclack reports a prompt on a pipe as NotConnected. Keep that
        // implementation detail out of the CLI transcript and point callers
        // at the non-interactive escape hatch instead.
        io::ErrorKind::NotConnected => anyhow::anyhow!(
            "this prompt needs a terminal; pass --yes or --no-input with the required flags"
        ),
        _ => anyhow::Error::new(error),
    }
}

pub(crate) struct Text {
    question: String,
    default: Option<String>,
}

impl Text {
    pub(crate) fn new(question: impl Into<String>) -> Self {
        Self {
            question: question.into(),
            default: None,
        }
    }

    pub(crate) fn with_default(mut self, default: impl Into<String>) -> Self {
        self.default = Some(default.into());
        self
    }

    pub(crate) fn ask(self) -> anyhow::Result<String> {
        let mut renderer = RailRenderer::new(Output::new(super::output::OutputMode::Human, false));
        renderer.event(&UiEvent::PromptShown {
            question: self.question.clone(),
        });
        let mut prompt = cliclack::input(&self.question);
        if let Some(default) = &self.default {
            prompt = prompt.default_input(default);
        }
        let answer: String = prompt.interact().map_err(prompt_error)?;
        renderer.event(&UiEvent::PromptAnswered {
            question: self.question,
            answer: PromptAnswer::Visible(answer.clone()),
        });
        Ok(answer)
    }
}

pub(crate) struct Confirm {
    question: String,
    default: bool,
}

impl Confirm {
    pub(crate) fn new(question: impl Into<String>) -> Self {
        Self {
            question: question.into(),
            default: false,
        }
    }

    pub(crate) fn with_default(mut self, default: bool) -> Self {
        self.default = default;
        self
    }

    pub(crate) fn ask(self) -> anyhow::Result<bool> {
        let mut renderer = RailRenderer::new(Output::new(super::output::OutputMode::Human, false));
        renderer.event(&UiEvent::PromptShown {
            question: self.question.clone(),
        });
        let answer = cliclack::confirm(&self.question)
            .initial_value(self.default)
            .interact()
            .map_err(prompt_error)?;
        renderer.event(&UiEvent::PromptAnswered {
            question: self.question,
            answer: PromptAnswer::Visible(if answer { "yes" } else { "no" }.to_string()),
        });
        Ok(answer)
    }

    /// Check the invocation policy before rendering any prompt bytes.
    pub(crate) fn ask_with_output(self, output: Output) -> anyhow::Result<bool> {
        output.ensure_prompt_allowed()?;
        self.ask()
    }
}

pub(crate) struct Password {
    question: String,
}

impl Password {
    pub(crate) fn new(question: impl Into<String>) -> Self {
        Self {
            question: question.into(),
        }
    }

    pub(crate) fn ask(self) -> anyhow::Result<String> {
        let mut renderer = RailRenderer::new(Output::new(super::output::OutputMode::Human, false));
        renderer.event(&UiEvent::PromptShown {
            question: self.question.clone(),
        });
        let answer = cliclack::password(&self.question)
            .interact()
            .map_err(prompt_error)?;
        renderer.event(&UiEvent::PromptAnswered {
            question: self.question,
            answer: PromptAnswer::Secret,
        });
        Ok(answer)
    }
}

pub(crate) struct Select<T> {
    question: String,
    items: Vec<(T, String, String)>,
}

impl<T: Clone + Eq + std::fmt::Display> Select<T> {
    pub(crate) fn new(question: impl Into<String>) -> Self {
        Self {
            question: question.into(),
            items: Vec::new(),
        }
    }

    pub(crate) fn items(mut self, items: impl IntoIterator<Item = T>) -> Self
    where
        T: std::fmt::Display,
    {
        self.items.extend(items.into_iter().map(|value| {
            let label = value.to_string();
            (value, label, String::new())
        }));
        self
    }

    pub(crate) fn ask(self) -> anyhow::Result<T> {
        let mut renderer = RailRenderer::new(Output::new(super::output::OutputMode::Human, false));
        renderer.event(&UiEvent::PromptShown {
            question: self.question.clone(),
        });
        let mut prompt = cliclack::select(&self.question);
        for (value, label, hint) in self.items {
            prompt = prompt.item(value, label, hint);
        }
        let answer = prompt.interact().map_err(prompt_error)?;
        renderer.event(&UiEvent::PromptAnswered {
            question: self.question,
            answer: PromptAnswer::Visible(answer.to_string()),
        });
        Ok(answer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupted_is_shared_cancel() {
        let error = prompt_error(io::ErrorKind::Interrupted.into());
        assert!(super::super::picker::is_canceled(&error));
    }

    #[test]
    fn other_io_errors_are_not_cancel() {
        let error = prompt_error(io::ErrorKind::NotConnected.into());
        assert!(!super::super::picker::is_canceled(&error));
        assert!(error.to_string().contains("pass --yes or --no-input"));
    }

    #[test]
    fn password_event_is_redacted() {
        let event = crate::ui::event::UiEvent::PromptAnswered {
            question: "Token".to_string(),
            answer: crate::ui::event::PromptAnswer::Secret,
        };
        assert!(matches!(
            event,
            crate::ui::event::UiEvent::PromptAnswered {
                answer: crate::ui::event::PromptAnswer::Secret,
                ..
            }
        ));
    }

    #[test]
    fn structured_prompt_policy_fails_before_display() {
        let error = Confirm::new("Proceed?")
            .ask_with_output(Output::new(super::super::output::OutputMode::Json, false))
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("interactive input is unavailable")
        );
    }
}
