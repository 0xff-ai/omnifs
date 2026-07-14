//! Invocation-owned output policy for the machine contract.
//!
//! [`Output`] owns mode, quiet, prompt, and command-path policy for one
//! invocation. Commands clone it and pass it to short-lived progress handles.
//!
//! No command should add another boolean cluster or process-global switch.

use serde::Serialize;
use std::fmt::Write as _;
use std::io::{self, Write};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use super::event::{JsonlError, JsonlEvent, JsonlResult};
use super::style::Glyph;

pub(crate) const SCHEMA_VERSION: u8 = 1;

struct DefaultTheme;
impl cliclack::Theme for DefaultTheme {}

struct OmnifsTheme;

impl cliclack::Theme for OmnifsTheme {
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
            format!("│\n{symbol} {}\n", super::style::heading(first))
        };
        for line in lines {
            let _ = writeln!(out, "│  {line}");
        }
        out
    }

    fn format_header(&self, state: &cliclack::ThemeState, prompt: &str) -> String {
        if matches!(state, cliclack::ThemeState::Cancel) {
            String::new()
        } else {
            format!(
                "│\n{}",
                <DefaultTheme as cliclack::Theme>::format_header(&DefaultTheme, state, prompt)
            )
        }
    }

    fn format_footer(&self, state: &cliclack::ThemeState) -> String {
        if matches!(state, cliclack::ThemeState::Cancel) {
            String::new()
        } else {
            <DefaultTheme as cliclack::Theme>::format_footer(&DefaultTheme, state)
        }
    }

    fn format_input(
        &self,
        state: &cliclack::ThemeState,
        cursor: &cliclack::StringCursor,
    ) -> String {
        if matches!(state, cliclack::ThemeState::Cancel) {
            String::new()
        } else {
            <DefaultTheme as cliclack::Theme>::format_input(&DefaultTheme, state, cursor)
        }
    }

    fn format_placeholder(
        &self,
        state: &cliclack::ThemeState,
        cursor: &cliclack::StringCursor,
    ) -> String {
        if matches!(state, cliclack::ThemeState::Cancel) {
            String::new()
        } else {
            <DefaultTheme as cliclack::Theme>::format_placeholder(&DefaultTheme, state, cursor)
        }
    }

    fn format_select_item(
        &self,
        state: &cliclack::ThemeState,
        selected: bool,
        label: &str,
        hint: &str,
    ) -> String {
        if matches!(state, cliclack::ThemeState::Cancel) {
            String::new()
        } else {
            <DefaultTheme as cliclack::Theme>::format_select_item(
                &DefaultTheme,
                state,
                selected,
                label,
                hint,
            )
        }
    }

    fn format_confirm(&self, state: &cliclack::ThemeState, confirm: bool) -> String {
        if matches!(state, cliclack::ThemeState::Cancel) {
            String::new()
        } else {
            <DefaultTheme as cliclack::Theme>::format_confirm(&DefaultTheme, state, confirm)
        }
    }
}
pub(crate) fn install_theme() {
    cliclack::set_theme(OmnifsTheme);
}

#[derive(Debug, Default)]
struct OutputState {
    terminal: bool,
    closed: bool,
    failure: Option<String>,
}

#[derive(Debug)]
struct OutputFailure(String);

impl std::fmt::Display for OutputFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for OutputFailure {}

fn state(output: &Output) -> MutexGuard<'_, OutputState> {
    output
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Verdict for a completed command result. Degraded is a successful terminal
/// document with actionable resources, not an error envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ResultVerdict {
    Ok,
    Degraded,
}

/// Verdict for a terminal error document. Cancellation is kept distinct from
/// failures so agents can handle Ctrl-C without treating it as a crash.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ErrorVerdict {
    Failed,
    Canceled,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ResultEnvelope<T> {
    pub(crate) schema_version: u8,
    pub(crate) command: String,
    pub(crate) verdict: ResultVerdict,
    pub(crate) result: T,
}

impl<T> ResultEnvelope<T> {
    pub(crate) fn new(command: impl Into<String>, verdict: ResultVerdict, result: T) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            command: command.into(),
            verdict,
            result,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ErrorEnvelope {
    pub(crate) schema_version: u8,
    pub(crate) command: String,
    pub(crate) verdict: ErrorVerdict,
    pub(crate) error: ErrorPayload,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ErrorPayload {
    pub(crate) id: String,
    pub(crate) exit_code: i32,
    pub(crate) message: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) causes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) fix: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) hints: Vec<String>,
}

impl ErrorEnvelope {
    pub(crate) fn new(
        command: impl Into<String>,
        verdict: ErrorVerdict,
        payload: ErrorPayload,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            command: command.into(),
            verdict,
            error: payload,
        }
    }

    /// The last-resort error document used when a command's result cannot be
    /// serialized.  It deliberately contains only fixed, primitive fields so
    /// constructing this fallback never recurses through the failing result.
    pub(crate) fn serialization_failure(command: impl Into<String>) -> Self {
        Self::new(
            command,
            ErrorVerdict::Failed,
            ErrorPayload {
                id: "serialization-failed".to_owned(),
                exit_code: 1,
                message: "failed to serialize structured output".to_owned(),
                causes: Vec::new(),
                fix: None,
                hints: Vec::new(),
            },
        )
    }
}

impl Output {
    /// Serialize a terminal result without touching stdout or process-global
    /// state. JSONL uses the same terminal object shape as JSON.
    pub(crate) fn result_bytes<T: Serialize>(
        command: impl Into<String>,
        verdict: ResultVerdict,
        result: T,
    ) -> serde_json::Result<Vec<u8>> {
        serde_json::to_vec(&ResultEnvelope::new(command, verdict, result))
    }

    pub(crate) fn error_bytes(error: &ErrorEnvelope) -> serde_json::Result<Vec<u8>> {
        serde_json::to_vec(error)
    }

    pub(crate) fn event_bytes(
        mode: OutputMode,
        event: &JsonlEvent,
    ) -> serde_json::Result<Option<Vec<u8>>> {
        if mode == OutputMode::Jsonl {
            serde_json::to_vec(event).map(Some)
        } else {
            Ok(None)
        }
    }

    pub(crate) fn jsonl_result_bytes<T: Serialize>(
        command: impl Into<String>,
        verdict: ResultVerdict,
        result: T,
    ) -> serde_json::Result<Vec<u8>> {
        serde_json::to_vec(&JsonlResult::new(command, verdict, result))
    }

    pub(crate) fn jsonl_error_bytes(error: ErrorEnvelope) -> serde_json::Result<Vec<u8>> {
        serde_json::to_vec(&JsonlError::from_envelope(error))
    }

    pub(crate) fn write_bytes<W: Write>(writer: &mut W, bytes: &[u8]) -> std::io::Result<()> {
        writer.write_all(bytes)?;
        writer.write_all(b"\n")
    }

    /// Write one terminal result, falling back to a minimal error envelope if
    /// serializing the result itself fails. The return value tells callers
    /// whether the emitted terminal line was a result (`true`) or the
    /// deterministic serialization error (`false`), so they can preserve the
    /// corresponding exit status without emitting a second document.
    pub(crate) fn write_result_with_fallback<W: Write, T: Serialize>(
        &self,
        writer: &mut W,
        command: impl Into<String>,
        verdict: ResultVerdict,
        result: T,
    ) -> anyhow::Result<bool> {
        if self.mode == OutputMode::Human {
            anyhow::bail!("structured terminal output is unavailable in human mode");
        }
        let command = command.into();
        let bytes = match self.mode {
            OutputMode::Json => {
                if let Ok(bytes) = Self::result_bytes(command.clone(), verdict, result) {
                    bytes
                } else {
                    let error = ErrorEnvelope::serialization_failure(command);
                    self.write_error(writer, error)?;
                    return Ok(false);
                }
            },
            OutputMode::Jsonl => {
                if let Ok(bytes) = Self::jsonl_result_bytes(command.clone(), verdict, result) {
                    bytes
                } else {
                    let error = ErrorEnvelope::serialization_failure(command);
                    self.write_error(writer, error)?;
                    return Ok(false);
                }
            },
            OutputMode::Human => unreachable!("human mode checked above"),
        };
        Self::write_bytes(writer, &bytes)?;
        Ok(true)
    }

    pub(crate) fn write_error<W: Write>(
        &self,
        writer: &mut W,
        error: ErrorEnvelope,
    ) -> anyhow::Result<()> {
        if self.mode == OutputMode::Human {
            anyhow::bail!("structured terminal output is unavailable in human mode");
        }
        let bytes = if self.mode == OutputMode::Jsonl {
            Self::jsonl_error_bytes(error)?
        } else {
            Self::error_bytes(&error)?
        };
        Self::write_bytes(writer, &bytes)?;
        Ok(())
    }

    pub(crate) fn write_event<W: Write>(
        &self,
        writer: &mut W,
        event: &JsonlEvent,
    ) -> anyhow::Result<bool> {
        let Some(bytes) = Self::event_bytes(self.mode, event)? else {
            return Ok(false);
        };
        Self::write_bytes(writer, &bytes)?;
        Ok(true)
    }

    /// Structured modes and explicit no-input policy reject prompts before a
    /// prompt renderer can print a question.
    pub(crate) fn ensure_prompt_allowed(&self) -> anyhow::Result<()> {
        if self.no_input || self.mode.is_structured() {
            anyhow::bail!("interactive input is unavailable in structured or no-input mode")
        }
        Ok(())
    }
}

/// Output policy owned by one CLI invocation. Commands clone this handle and
/// pass it through short-lived progress handles instead of consulting
/// process-global switches.
#[derive(Debug, Clone)]
pub(crate) struct Output {
    mode: OutputMode,
    quiet: bool,
    no_input: bool,
    yes: bool,
    command: &'static str,
    state: Arc<Mutex<OutputState>>,
}

impl Output {
    pub(crate) fn new(mode: OutputMode, quiet: bool) -> Self {
        Self {
            mode,
            quiet,
            no_input: false,
            yes: false,
            command: "invocation",
            state: Arc::new(Mutex::new(OutputState::default())),
        }
    }

    pub(crate) const fn mode(&self) -> OutputMode {
        self.mode
    }

    pub(crate) const fn is_structured(&self) -> bool {
        self.mode.is_structured()
    }

    pub(crate) const fn quiet(&self) -> bool {
        self.quiet
    }

    pub(crate) const fn no_input(&self) -> bool {
        self.no_input
    }

    pub(crate) const fn yes(&self) -> bool {
        self.yes
    }

    pub(crate) const fn command(&self) -> &'static str {
        self.command
    }

    pub(crate) const fn with_command(mut self, command: &'static str) -> Self {
        self.command = command;
        self
    }

    /// Optional narration belongs to the invocation policy: it is human-only
    /// and quiet suppresses it, while structured streams stay machine-clean.
    pub(crate) fn narrate(&self, line: impl std::fmt::Display) {
        if self.mode == OutputMode::Human && !self.quiet {
            let _ = cliclack::log::remark(crate::ui::style::accentuate(&line.to_string()));
        }
    }

    pub(crate) fn note(&self, line: impl std::fmt::Display) {
        self.narrate(line);
    }

    pub(crate) fn answer(&self, question: &str, answer: impl std::fmt::Display) {
        if self.mode == OutputMode::Human && !self.quiet {
            let _ = cliclack::log::remark(format!(
                "{} {question} {}",
                Glyph::Done.render(),
                crate::ui::style::accent(answer)
            ));
        }
    }

    pub(crate) fn intro(&self, title: impl std::fmt::Display) -> anyhow::Result<()> {
        if self.mode == OutputMode::Human && !self.quiet {
            cliclack::intro(title)?;
        }
        Ok(())
    }

    pub(crate) fn phase(&self, title: impl Into<String>) {
        let title = title.into();
        if self.mode == OutputMode::Human {
            if !self.quiet {
                let _ = cliclack::log::step(&title);
            }
        } else if self.mode == OutputMode::Jsonl {
            self.emit_event(JsonlEvent::Phase(super::event::JsonlPhase::new(
                self.command,
                title,
                "started",
            )));
        }
    }

    pub(crate) fn row(&self, row: super::report::Row) {
        if self.mode == OutputMode::Human {
            let _ = cliclack::log::remark(row.render().trim_start());
        }
    }

    pub(crate) fn plan(&self, plan: &super::consent::Plan) {
        if self.mode != OutputMode::Human {
            return;
        }
        let _ = cliclack::log::step("plan");
        let rows = plan
            .rows
            .iter()
            .map(super::consent::Row::render_plan)
            .collect::<Vec<_>>();
        let _ = cliclack::log::remark(super::report::render_rows(&rows));
        let _ = cliclack::log::remark(crate::ui::style::dim(plan.summary()));
    }

    pub(crate) fn receipt(&self, receipt: &super::consent::Receipt) {
        if self.mode != OutputMode::Human {
            return;
        }
        let _ = cliclack::log::step("apply");
        let rows = receipt
            .rows
            .iter()
            .map(super::consent::Outcome::render_receipt)
            .collect::<Vec<_>>();
        let _ = cliclack::log::remark(super::report::render_rows(&rows));
    }

    pub(crate) fn outro(&self, message: impl Into<String>) {
        let mut current = state(self);
        if current.closed {
            return;
        }
        current.closed = true;
        drop(current);
        if self.mode == OutputMode::Human && !self.quiet {
            let _ = cliclack::outro(message.into());
        }
    }

    pub(crate) fn progress(
        &self,
        length: u64,
        message: impl std::fmt::Display,
    ) -> crate::ui::progress::Progress {
        crate::ui::progress::Progress::start(self.clone(), length, message)
    }

    pub(crate) fn progress_event(&self, resource: String, message: String, elapsed: Duration) {
        if self.mode == OutputMode::Jsonl {
            self.emit_event(JsonlEvent::Progress(super::event::JsonlProgress::new(
                self.command,
                resource,
                message,
                u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            )));
        }
    }

    pub(crate) const fn with_no_input(mut self, no_input: bool) -> Self {
        self.no_input = no_input;
        self
    }

    pub(crate) const fn with_yes(mut self, yes: bool) -> Self {
        self.yes = yes;
        self
    }

    fn failure(&self) -> Option<anyhow::Error> {
        state(self)
            .failure
            .as_ref()
            .map(|message| anyhow::Error::new(OutputFailure(message.clone())))
    }

    fn fail(&self, error: impl std::fmt::Display) {
        let mut current = state(self);
        if current.failure.is_none() {
            current.failure = Some(error.to_string());
        }
    }

    fn ensure_terminal_open(&self) -> anyhow::Result<()> {
        if let Some(error) = self.failure() {
            return Err(error);
        }
        if state(self).terminal {
            anyhow::bail!("terminal output has already been settled")
        }
        Ok(())
    }

    fn mark_terminal(&self) {
        state(self).terminal = true;
    }

    fn emit_event(&self, event: JsonlEvent) {
        if let Err(error) = self.write_event_to_stdout(&event) {
            self.fail(error);
        }
    }

    fn write_event_to_stdout(&self, event: &JsonlEvent) -> anyhow::Result<()> {
        self.ensure_terminal_open()?;
        let mut stdout = io::stdout().lock();
        self.write_event(&mut stdout, event)?;
        Ok(())
    }

    fn settle_result<W: Write, T: Serialize>(
        &self,
        writer: &mut W,
        verdict: impl Into<ResultVerdict>,
        result: T,
    ) -> anyhow::Result<()> {
        if !self.mode.is_structured() {
            anyhow::bail!("structured terminal output is unavailable in human mode");
        }
        self.ensure_terminal_open()?;
        let emitted =
            self.write_result_with_fallback(writer, self.command(), verdict.into(), result);
        match emitted {
            Ok(true) => {
                self.mark_terminal();
                Ok(())
            },
            Ok(false) => {
                self.mark_terminal();
                anyhow::bail!("failed to serialize structured result")
            },
            Err(error) => {
                self.fail(&error);
                Err(error)
            },
        }
    }

    fn settle_error<W: Write>(&self, writer: &mut W, error: ErrorEnvelope) -> anyhow::Result<()> {
        if !self.mode.is_structured() {
            anyhow::bail!("structured terminal output is unavailable in human mode");
        }
        self.ensure_terminal_open()?;
        match self.write_error(writer, error) {
            Ok(()) => {
                self.mark_terminal();
                Ok(())
            },
            Err(error) => {
                self.fail(&error);
                Err(error)
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum OutputMode {
    Human,
    Json,
    Jsonl,
}

impl OutputMode {
    pub(crate) const fn is_human(self) -> bool {
        matches!(self, Self::Human)
    }

    pub(crate) const fn is_structured(self) -> bool {
        !matches!(self, Self::Human)
    }
}

impl Output {
    /// Emit one terminal result on stdout. Human output remains owned by the
    /// existing table/receipt renderers and never calls this method.
    pub(crate) fn emit_result<T: Serialize>(
        &self,
        verdict: impl Into<ResultVerdict>,
        result: T,
    ) -> anyhow::Result<()> {
        let mut stdout = io::stdout().lock();
        self.settle_result(&mut stdout, verdict, result)
    }

    pub(crate) fn emit_error(&self, error: ErrorEnvelope) -> anyhow::Result<()> {
        let mut stdout = io::stdout().lock();
        self.settle_error(&mut stdout, error)
    }
}

impl From<crate::inventory::Verdict> for ResultVerdict {
    fn from(verdict: crate::inventory::Verdict) -> Self {
        match verdict {
            crate::inventory::Verdict::Ok => Self::Ok,
            crate::inventory::Verdict::Degraded => Self::Degraded,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::event::{JsonlEvent, JsonlPhase, JsonlProgress};

    #[test]
    fn invocation_policy_is_cloneable_and_explicit() {
        let output = Output::new(OutputMode::Jsonl, true)
            .with_no_input(true)
            .with_yes(true);
        assert_eq!(output.clone().mode(), output.mode());
        assert_eq!(output.mode(), OutputMode::Jsonl);
        assert!(output.quiet());
        assert!(output.no_input());
        assert!(output.yes());
        assert!(output.mode().is_structured());
        assert!(!OutputMode::Human.is_structured());
    }

    #[test]
    fn result_bytes_have_stable_terminal_shape() {
        let bytes = Output::result_bytes(
            "status",
            ResultVerdict::Degraded,
            serde_json::json!({"mounts": 2}),
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "schema_version": 1,
                "command": "status",
                "verdict": "degraded",
                "result": {"mounts": 2}
            })
        );
    }

    #[test]
    fn json_mode_suppresses_events_but_jsonl_emits_tagged_lines() {
        let phase = JsonlEvent::Phase(JsonlPhase::new("up", "daemon", "started"));
        let progress = JsonlEvent::Progress(JsonlProgress::new(
            "up",
            "frontend:docker:fuse",
            "waiting",
            820,
        ));
        assert!(
            Output::event_bytes(OutputMode::Json, &phase)
                .unwrap()
                .is_none()
        );
        let bytes = Output::event_bytes(OutputMode::Jsonl, &phase)
            .unwrap()
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["type"], "phase");
        assert_eq!(value["schema_version"], 1);
        let bytes = Output::event_bytes(OutputMode::Jsonl, &progress)
            .unwrap()
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["type"], "progress");
        assert_eq!(value["elapsed_ms"], 820);
    }

    #[test]
    fn structured_modes_reject_prompt_before_display() {
        assert!(
            Output::new(OutputMode::Json, false)
                .ensure_prompt_allowed()
                .is_err()
        );
        assert!(
            Output::new(OutputMode::Human, false)
                .with_no_input(true)
                .ensure_prompt_allowed()
                .is_err()
        );
        assert!(
            Output::new(OutputMode::Human, false)
                .ensure_prompt_allowed()
                .is_ok()
        );
    }

    #[test]
    fn write_bytes_appends_one_newline_without_printing() {
        let mut bytes = Vec::new();
        Output::write_bytes(&mut bytes, br#"{"ok":true}"#).unwrap();
        assert_eq!(bytes, b"{\"ok\":true}\n");
    }

    #[test]
    fn write_paths_are_buffer_only_and_mode_aware() {
        let mut json = Vec::new();
        let output = Output::new(OutputMode::Json, false);
        output
            .write_result_with_fallback(
                &mut json,
                "status",
                ResultVerdict::Ok,
                serde_json::json!({}),
            )
            .unwrap();
        assert_eq!(std::str::from_utf8(&json).unwrap().matches('\n').count(), 1);

        let event = JsonlEvent::Phase(JsonlPhase::new("up", "daemon", "started"));
        assert!(!output.write_event(&mut json, &event).unwrap());
        assert_eq!(std::str::from_utf8(&json).unwrap().matches('\n').count(), 1);

        let mut jsonl = Vec::new();
        Output::new(OutputMode::Jsonl, false)
            .write_event(&mut jsonl, &event)
            .unwrap();
        assert_eq!(
            std::str::from_utf8(&jsonl).unwrap().matches('\n').count(),
            1
        );
    }

    #[test]
    fn human_mode_rejects_structured_terminal_writes() {
        let mut bytes = Vec::new();
        let result = Output::new(OutputMode::Human, false).write_result_with_fallback(
            &mut bytes,
            "status",
            ResultVerdict::Ok,
            serde_json::json!({}),
        );
        assert!(result.is_err());
        assert!(bytes.is_empty());

        let error = ErrorEnvelope::serialization_failure("status");
        let result = Output::new(OutputMode::Human, false).write_error(&mut bytes, error);
        assert!(result.is_err());
        assert!(bytes.is_empty());
    }

    #[test]
    fn serialization_fallback_is_one_minimal_terminal_error() {
        struct Fails;
        impl serde::Serialize for Fails {
            fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                Err(serde::ser::Error::custom("boom"))
            }
        }

        let mut json = Vec::new();
        let emitted = Output::new(OutputMode::Json, false)
            .write_result_with_fallback(&mut json, "status", ResultVerdict::Ok, Fails)
            .unwrap();
        assert!(!emitted);
        let value: serde_json::Value =
            serde_json::from_slice(json.strip_suffix(b"\n").unwrap()).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "schema_version": 1,
                "command": "status",
                "verdict": "failed",
                "error": {
                    "id": "serialization-failed",
                    "exit_code": 1,
                    "message": "failed to serialize structured output"
                }
            })
        );

        let mut jsonl = Vec::new();
        let emitted = Output::new(OutputMode::Jsonl, false)
            .write_result_with_fallback(&mut jsonl, "status", ResultVerdict::Ok, Fails)
            .unwrap();
        assert!(!emitted);
        let value: serde_json::Value =
            serde_json::from_slice(jsonl.strip_suffix(b"\n").unwrap()).unwrap();
        assert_eq!(value["type"], "error");
        assert_eq!(value["verdict"], "failed");
        assert_eq!(value["error"]["id"], "serialization-failed");
    }

    #[test]
    fn terminal_settlement_is_single_and_writer_failure_is_sticky() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("broken stdout"))
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let output = Output::new(OutputMode::Jsonl, false).with_command("status");
        let mut broken = Broken;
        assert!(
            output
                .settle_result(&mut broken, ResultVerdict::Ok, serde_json::json!({}))
                .is_err()
        );

        let mut bytes = Vec::new();
        let error = output
            .settle_result(&mut bytes, ResultVerdict::Ok, serde_json::json!({}))
            .unwrap_err();
        assert!(error.to_string().contains("broken stdout"));
        assert!(bytes.is_empty());
    }

    #[test]
    fn terminal_settlement_rejects_a_second_document() {
        let output = Output::new(OutputMode::Json, false).with_command("status");
        let mut bytes = Vec::new();
        output
            .settle_result(&mut bytes, ResultVerdict::Ok, serde_json::json!({}))
            .unwrap();
        assert!(
            output
                .settle_error(&mut bytes, ErrorEnvelope::serialization_failure("status"),)
                .is_err()
        );
        assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 1);
    }
}
