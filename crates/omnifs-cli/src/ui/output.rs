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

type OutputWriter = Box<dyn Write + Send>;

impl OutputState {
    fn sticky_error(&self) -> Option<anyhow::Error> {
        self.failure
            .as_ref()
            .map(|message| anyhow::Error::new(OutputFailure(message.clone())))
    }
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

fn stdout(output: &Output) -> MutexGuard<'_, OutputWriter> {
    output
        .stdout
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

#[derive(Debug, Clone, Serialize)]
struct JsonlResult<T> {
    schema_version: u8,
    #[serde(rename = "type")]
    kind: &'static str,
    command: String,
    verdict: ResultVerdict,
    result: T,
}

impl<T> JsonlResult<T> {
    fn new(command: impl Into<String>, verdict: ResultVerdict, result: T) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            kind: "result",
            command: command.into(),
            verdict,
            result,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct JsonlError {
    schema_version: u8,
    #[serde(rename = "type")]
    kind: &'static str,
    command: String,
    verdict: ErrorVerdict,
    error: ErrorPayload,
}

impl JsonlError {
    fn from_envelope(envelope: ErrorEnvelope) -> Self {
        Self {
            schema_version: envelope.schema_version,
            kind: "error",
            command: envelope.command,
            verdict: envelope.verdict,
            error: envelope.error,
        }
    }
}

impl Output {
    /// Serialize the JSON terminal result envelope without touching stdout or
    /// process-global state. JSONL adds a `"type":"result"` discriminator
    /// around its terminal representation.
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
#[derive(Clone)]
pub(crate) struct Output {
    mode: OutputMode,
    quiet: bool,
    no_input: bool,
    yes: bool,
    command: &'static str,
    state: Arc<Mutex<OutputState>>,
    stdout: Arc<Mutex<OutputWriter>>,
}

impl std::fmt::Debug for Output {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Output")
            .field("mode", &self.mode)
            .field("quiet", &self.quiet)
            .field("no_input", &self.no_input)
            .field("yes", &self.yes)
            .field("command", &self.command)
            .finish_non_exhaustive()
    }
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
            stdout: Arc::new(Mutex::new(Box::new(io::stdout()))),
        }
    }

    #[cfg(test)]
    fn with_writer(mut self, writer: impl Write + Send + 'static) -> Self {
        self.stdout = Arc::new(Mutex::new(Box::new(writer)));
        self
    }

    pub(crate) const fn is_structured(&self) -> bool {
        self.mode.is_structured()
    }

    pub(crate) fn show_progress(&self) -> bool {
        self.mode == OutputMode::Human && !self.quiet
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

    pub(crate) fn row(&self, row: &super::report::Row) {
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

    pub(crate) fn progress(&self, key: impl Into<String>) -> crate::ui::progress::Progress {
        crate::ui::progress::Progress::new(self.clone(), key)
    }

    pub(crate) const fn with_no_input(mut self, no_input: bool) -> Self {
        self.no_input = no_input;
        self
    }

    pub(crate) const fn with_yes(mut self, yes: bool) -> Self {
        self.yes = yes;
        self
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
        let mut current = state(self);
        self.settle_result_locked(&mut current, writer, verdict, result)
    }

    fn settle_result_locked<W: Write, T: Serialize>(
        &self,
        current: &mut OutputState,
        writer: &mut W,
        verdict: impl Into<ResultVerdict>,
        result: T,
    ) -> anyhow::Result<()> {
        if !self.mode.is_structured() {
            anyhow::bail!("structured terminal output is unavailable in human mode");
        }
        if let Some(error) = current.sticky_error() {
            return Err(error);
        }
        if current.terminal {
            anyhow::bail!("terminal output has already been settled")
        }
        let emitted =
            self.write_result_with_fallback(writer, self.command(), verdict.into(), result);
        match emitted {
            Ok(true) => {
                current.terminal = true;
                Ok(())
            },
            Ok(false) => {
                current.terminal = true;
                anyhow::bail!("failed to serialize structured result")
            },
            Err(error) => {
                current.failure = Some(error.to_string());
                Err(error)
            },
        }
    }

    fn settle_error_locked<W: Write>(
        &self,
        current: &mut OutputState,
        writer: &mut W,
        error: ErrorEnvelope,
    ) -> anyhow::Result<()> {
        if !self.mode.is_structured() {
            anyhow::bail!("structured terminal output is unavailable in human mode");
        }
        if let Some(error) = current.sticky_error() {
            return Err(error);
        }
        if current.terminal {
            anyhow::bail!("terminal output has already been settled")
        }
        match self.write_error(writer, error) {
            Ok(()) => {
                current.terminal = true;
                Ok(())
            },
            Err(error) => {
                current.failure = Some(error.to_string());
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
        let mut current = state(self);
        let mut stdout = stdout(self);
        self.settle_result_locked(&mut current, &mut *stdout, verdict, result)
    }

    pub(crate) fn emit_error(&self, error: ErrorEnvelope) -> anyhow::Result<()> {
        let mut current = state(self);
        let mut stdout = stdout(self);
        self.settle_error_locked(&mut current, &mut *stdout, error)
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

    #[test]
    fn invocation_policy_is_cloneable_and_explicit() {
        let output = Output::new(OutputMode::Jsonl, true)
            .with_no_input(true)
            .with_yes(true);
        assert!(output.no_input());
        assert!(output.yes());
        assert!(output.is_structured());
        assert!(!output.show_progress());
        assert!(!Output::new(OutputMode::Json, false).show_progress());
        assert!(Output::new(OutputMode::Human, false).show_progress());
        assert!(!Output::new(OutputMode::Human, true).show_progress());
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

        let mut jsonl = Vec::new();
        Output::new(OutputMode::Jsonl, false)
            .write_result_with_fallback(
                &mut jsonl,
                "status",
                ResultVerdict::Ok,
                serde_json::json!({}),
            )
            .unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(jsonl.strip_suffix(b"\n").unwrap()).unwrap();
        assert_eq!(value["type"], "result");
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
    fn concurrent_terminal_clones_share_one_stdout_lock() {
        use std::sync::{Arc, Barrier, mpsc};
        use std::thread;

        struct SharedWriter(Arc<Mutex<Vec<u8>>>);

        impl Write for SharedWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let bytes = Arc::new(Mutex::new(Vec::new()));
        let output = Output::new(OutputMode::Jsonl, false)
            .with_command("status")
            .with_writer(SharedWriter(Arc::clone(&bytes)));
        let barrier = Arc::new(Barrier::new(2));
        let (sender, receiver) = mpsc::channel();
        thread::scope(|scope| {
            let error_output = output.clone();
            let error_barrier = Arc::clone(&barrier);
            let error_sender = sender.clone();
            scope.spawn(move || {
                error_barrier.wait();
                error_sender
                    .send(error_output.emit_error(ErrorEnvelope::serialization_failure("status")))
                    .unwrap();
            });

            let terminal_output = output.clone();
            let terminal_barrier = Arc::clone(&barrier);
            let terminal_sender = sender;
            scope.spawn(move || {
                terminal_barrier.wait();
                terminal_sender
                    .send(terminal_output.emit_result(ResultVerdict::Ok, serde_json::json!({})))
                    .unwrap();
            });
        });

        let outcomes = [receiver.recv().unwrap(), receiver.recv().unwrap()];
        assert_eq!(outcomes.iter().filter(|result| result.is_ok()).count(), 1);
        let lines = String::from_utf8(bytes.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(lines.len(), 1);
        assert!(matches!(
            lines[0]["type"].as_str(),
            Some("result" | "error")
        ));
    }
}
