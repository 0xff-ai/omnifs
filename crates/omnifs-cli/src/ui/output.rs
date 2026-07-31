//! Invocation-owned output policy for the machine contract.
//!
//! [`Output`] owns mode, quiet, prompt, and command-path policy for one
//! invocation. Commands clone it instead of consulting process-global state.
//!
//! No command should add another boolean cluster or process-global switch.

use anyhow::Context as _;
use serde::Serialize;
use std::io::{self, Write};
use std::sync::{Arc, Mutex, MutexGuard};

pub(crate) const SCHEMA_VERSION: u8 = 1;

/// Real terminal capabilities for the flat renderer (`render.rs`), read fresh
/// per call rather than cached: a prompt can change terminal state (raw mode,
/// size) between one narration line and the next.
/// `pub(crate)` so the top-level error boundary (`error.rs`) can build the
/// same stderr capabilities the rest of this module's narration uses.
///
/// Mirrors `render.rs::stdout_capabilities`'s is-tty gate: piped stderr gets
/// the stable 120-column width, never the `crossterm::terminal::size` error
/// fallback of 80, which would word-wrap content mid-path (a real path or
/// command embedded in a sentence) the moment stderr is redirected.
pub(crate) fn stderr_capabilities(_quiet: bool) -> super::render::Capabilities {
    let (_is_tty, width, color) = super::style::probe(super::style::Stream::Stderr);
    super::render::Capabilities { width, color }
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

#[derive(Debug, Clone, Serialize)]
struct JsonlEvent<T> {
    schema_version: u8,
    #[serde(rename = "type")]
    kind: &'static str,
    command: String,
    event: T,
}

impl<T> JsonlEvent<T> {
    fn new(command: impl Into<String>, event: T) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            kind: "event",
            command: command.into(),
            event,
        }
    }
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

#[derive(Debug, Clone, Serialize)]
struct DetailedErrorEnvelope<T> {
    schema_version: u8,
    command: String,
    verdict: ErrorVerdict,
    error: DetailedErrorPayload<T>,
}

#[derive(Debug, Clone, Serialize)]
struct JsonlDetailedError<T> {
    schema_version: u8,
    #[serde(rename = "type")]
    kind: &'static str,
    command: String,
    verdict: ErrorVerdict,
    error: DetailedErrorPayload<T>,
}

#[derive(Debug, Clone, Serialize)]
struct DetailedErrorPayload<T> {
    id: String,
    exit_code: i32,
    message: String,
    fix: String,
    details: T,
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
    /// Write daemon-owned passthrough bytes without text decoding or a line
    /// terminator. This is reserved for byte streams such as `omnifs logs`.
    pub(crate) fn write_raw_bytes(&self, bytes: &[u8]) -> anyhow::Result<()> {
        let mut stdout = stdout(self);
        stdout
            .write_all(bytes)
            .context("write passthrough output")?;
        stdout.flush().context("flush passthrough output")?;
        Ok(())
    }

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

    pub(crate) fn jsonl_event_bytes<T: Serialize>(
        command: impl Into<String>,
        event: T,
    ) -> serde_json::Result<Vec<u8>> {
        serde_json::to_vec(&JsonlEvent::new(command, event))
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

    /// Structured modes, explicit no-input policy, and the absence of a real
    /// terminal all reject prompts before a prompt renderer can print a
    /// question. Every prompt site must route through this (or
    /// [`Self::prompt_mode`], which consults the same predicate) rather than
    /// drawing a prompt frame directly: that is the one thing standing
    /// between a scripted `--output json` run and a hung terminal read.
    pub(crate) fn ensure_prompt_allowed(&self) -> anyhow::Result<()> {
        if !self.interactive() {
            anyhow::bail!("interactive input is unavailable in structured or no-input mode")
        }
        Ok(())
    }

    /// Whether this invocation may draw an interactive prompt at all: a real
    /// terminal, without `--no-input`, and outside every structured output
    /// mode. The one predicate every prompt-adjacent decision in this crate
    /// derives from, so a structured run and a piped run fail the same way
    /// regardless of which command reaches the check.
    pub(crate) fn interactive(&self) -> bool {
        self.interactive_on(crate::ui::prompt::is_terminal())
    }

    /// [`Self::interactive`]'s policy with the terminal probe supplied rather
    /// than read from the process, so the whole matrix stays provable without
    /// depending on whether the test runner happens to own a tty.
    fn interactive_on(&self, terminal: bool) -> bool {
        terminal && !self.no_input && !self.is_structured()
    }

    /// The prompt policy this invocation grants its guided flows: an explicit
    /// value always wins, `--yes` take the default, otherwise
    /// [`Self::interactive`] decides whether a flag hint or a real prompt
    /// follows. Built fresh from live invocation state so it can never drift
    /// from [`Self::ensure_prompt_allowed`]'s own predicate.
    pub(crate) fn prompt_mode(&self) -> PromptMode {
        PromptMode {
            interactive: self.interactive(),
            yes: self.yes,
            no_input: self.no_input,
        }
    }
}

/// Prompt policy derived once per invocation from [`Output`] state
/// ([`Output::prompt_mode`]). Fields are private so no other call site can
/// hand-assemble a self-contradictory combination (such as `interactive:
/// true` alongside `no_input: true`, which the one constructor can never
/// produce).
#[derive(Debug, Clone, Copy)]
pub(crate) struct PromptMode {
    interactive: bool,
    yes: bool,
    no_input: bool,
}

impl PromptMode {
    pub(crate) const fn interactive(self) -> bool {
        self.interactive
    }

    pub(crate) const fn yes(self) -> bool {
        self.yes
    }

    pub(crate) const fn no_input(self) -> bool {
        self.no_input
    }

    /// The single decision combinator for every guided prompt site: an explicit
    /// value wins; `--yes` takes the default; `--no-input` and non-interactive
    /// runs bail with a flag hint; otherwise prompt.
    pub(crate) fn resolve<T>(
        self,
        explicit: Option<T>,
        default: impl FnOnce() -> T,
        flag_hint: &str,
        prompt: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        if let Some(value) = explicit {
            return Ok(value);
        }
        if self.yes {
            return Ok(default());
        }
        if self.no_input {
            if flag_hint == "--yes" {
                anyhow::bail!("`--no-input` needs --yes to accept the default");
            }
            anyhow::bail!("`--no-input` needs {flag_hint}, or pass --yes to accept the default");
        }
        if !self.interactive {
            if flag_hint == "--yes" {
                anyhow::bail!("this step needs a terminal; pass --yes");
            }
            anyhow::bail!("this step needs a terminal; pass {flag_hint} or --yes");
        }
        prompt()
    }

    /// Build an arbitrary (possibly real-terminal-independent) prompt policy
    /// for a unit test. Real code always derives [`PromptMode`] from
    /// [`Output::prompt_mode`]; this exists only so tests can exercise
    /// [`Self::resolve`]'s branching without a PTY.
    #[cfg(test)]
    pub(crate) const fn for_test(interactive: bool, yes: bool, no_input: bool) -> Self {
        Self {
            interactive,
            yes,
            no_input,
        }
    }
}

/// Output policy owned by one CLI invocation.
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

    /// Print a pre-rendered report to stdout: the command's actual product
    /// (a table, a receipt, a bare screen), as opposed to stderr narration.
    /// `--quiet` only suppresses narration, never the product, so a report
    /// prints under quiet exactly as it does without it. A no-op in
    /// structured modes, which emit their one JSON/JSONL envelope instead of
    /// a rendered report.
    pub(crate) fn report(&self, rendered: impl AsRef<str>) {
        if self.mode == OutputMode::Human {
            crate::ui::print_raw(rendered.as_ref());
        }
    }

    /// Bail with a consistent message when a passthrough command (one that
    /// hands its stdout to something else entirely: a shell-completion
    /// script, a skill install, a raw log stream) is asked for structured
    /// output it has no way to produce. `command` is the bare subcommand
    /// name as the operator typed it (`"logs"`, not `"omnifs logs"`).
    pub(crate) fn require_human(&self, command: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.is_structured(),
            "{command} is a passthrough command and only supports human output"
        );
        Ok(())
    }

    pub(crate) const fn mode(&self) -> OutputMode {
        self.mode
    }

    pub(crate) fn show_progress(&self) -> bool {
        self.mode == OutputMode::Human && !self.quiet
    }

    pub(crate) const fn no_input(&self) -> bool {
        self.no_input
    }

    pub(crate) const fn quiet(&self) -> bool {
        self.quiet
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
    /// A flat, ungated line: no gutter, no step marker, nothing that repeats
    /// once the terminal scrolls it away.
    pub(crate) fn narrate(&self, line: impl std::fmt::Display) {
        if self.mode != OutputMode::Human || self.quiet {
            return;
        }
        let text = line.to_string();
        let caps = stderr_capabilities(self.quiet);
        crate::ui::eprint_raw(&super::render::narration_line(&text, caps));
    }

    /// A bold section heading line: plain bold, never the accent color, so it reads as
    /// structure rather than something the user can type. Human-only; quiet
    /// suppresses it like every other narration line.
    pub(crate) fn heading(&self, text: impl Into<String>) {
        if self.mode == OutputMode::Human && !self.quiet {
            let caps = stderr_capabilities(self.quiet);
            crate::ui::eprint_raw(&format!("{}\n", super::render::heading(&text.into(), caps)));
        }
    }

    /// The durable echo a prompt leaves behind once it resolves: the question
    /// it asked, plus the answer in accent. No glyph, since this is a
    /// one-line fact, not a settled operation.
    pub(crate) fn answer(&self, question: &str, answer: impl std::fmt::Display) {
        if self.mode == OutputMode::Human && !self.quiet {
            let caps = stderr_capabilities(self.quiet);
            crate::ui::eprint_raw(&super::render::narration_line(
                &format!("{question} `{answer}`"),
                caps,
            ));
        }
    }

    /// Print one durable v2-register ledger row at an externally
    /// supplied key width, so a block whose rows are printed one at a time
    /// as async work settles still reads as one aligned unit rather than
    /// each row sizing its own key column.
    pub(crate) fn ledger_row(&self, row: &super::render::LedgerRow, key_width: usize) {
        if self.mode != OutputMode::Human {
            return;
        }
        let caps = stderr_capabilities(self.quiet);
        crate::ui::eprint_raw(&format!(
            "{}\n",
            super::render::ledger_row_line(row, key_width, caps)
        ));
    }

    /// Print the consent plan preview. `Plan` owns the row-mapping and block
    /// shape (`super::consent::Plan::render`); this method only owns the
    /// mode/quiet gate every human-only print in this invocation shares.
    pub(crate) fn plan(&self, plan: &super::consent::Plan) {
        if self.mode != OutputMode::Human {
            return;
        }
        let caps = stderr_capabilities(self.quiet);
        crate::ui::eprint_raw(&plan.render(caps));
    }

    /// The v2 register never repeats the command the user just typed, so
    /// there is no frame opener to print; this exists only to close out the
    /// invocation with a plain sentence.
    pub(crate) fn outro(&self, message: impl Into<String>) {
        let mut current = state(self);
        if current.closed {
            return;
        }
        current.closed = true;
        drop(current);
        if self.mode == OutputMode::Human && !self.quiet {
            let caps = stderr_capabilities(self.quiet);
            crate::ui::eprint_raw(&format!(
                "{}\n",
                super::render::sentence(&message.into(), caps)
            ));
        }
    }

    /// Mark the closing line as already printed, without printing anything
    /// here: a prompt cancellation prints its own dim `canceled` line in
    /// place, at the moment it happens, rather than through [`Self::outro`]'s
    /// sentence styling. Marking closed keeps that one print the only one:
    /// see [`Self::is_closed`].
    pub(crate) fn mark_closed(&self) {
        state(self).closed = true;
    }

    /// Whether this invocation already printed its own closing line (via
    /// [`Self::outro`] or [`Self::mark_closed`]). The top-level cancel
    /// handler checks this so neither a consent decline's `Kept everything
    /// as it was.` nor an in-place prompt cancellation's `canceled` line is
    /// followed by the generic top-level `canceled` fallback; that fallback
    /// is the backstop for a cancellation that reaches `main` without any
    /// prompt site having printed anything (a raw `Interrupted` I/O error
    /// caught before a prompt's own resolution match, for example).
    pub(crate) fn is_closed(&self) -> bool {
        state(self).closed
    }

    pub(crate) const fn with_no_input(mut self, no_input: bool) -> Self {
        self.no_input = no_input;
        self
    }

    pub(crate) const fn with_yes(mut self, yes: bool) -> Self {
        self.yes = yes;
        self
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

    /// Emit one non-terminal JSONL event. Human and JSON modes intentionally
    /// do nothing so callers can feed every typed progress update through one
    /// path without leaking structured bytes into those modes.
    pub(crate) fn emit_jsonl_event<T: Serialize>(&self, event: T) -> anyhow::Result<()> {
        if self.mode != OutputMode::Jsonl {
            return Ok(());
        }
        let mut current = state(self);
        if let Some(error) = current.sticky_error() {
            return Err(error);
        }
        if current.terminal {
            anyhow::bail!("terminal output has already been settled")
        }
        let bytes = Self::jsonl_event_bytes(self.command(), event)?;
        let mut output = stdout(self);
        if let Err(error) = Self::write_bytes(&mut *output, &bytes) {
            current.failure = Some(error.to_string());
            return Err(error.into());
        }
        Ok(())
    }

    /// Emit one command-specific terminal error whose typed details must
    /// survive a lost watch connection after a durable daemon commit.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn emit_detailed_error<T: Serialize>(
        &self,
        verdict: ErrorVerdict,
        id: impl Into<String>,
        exit_code: i32,
        message: impl Into<String>,
        fix: impl Into<String>,
        details: T,
    ) -> anyhow::Result<()> {
        if !self.is_structured() {
            anyhow::bail!("structured terminal output is unavailable in human mode");
        }
        let mut current = state(self);
        if let Some(error) = current.sticky_error() {
            return Err(error);
        }
        if current.terminal {
            anyhow::bail!("terminal output has already been settled")
        }
        let payload = DetailedErrorPayload {
            id: id.into(),
            exit_code,
            message: message.into(),
            fix: fix.into(),
            details,
        };
        let serialized = match self.mode {
            OutputMode::Json => serde_json::to_vec(&DetailedErrorEnvelope {
                schema_version: SCHEMA_VERSION,
                command: self.command().to_owned(),
                verdict,
                error: payload,
            }),
            OutputMode::Jsonl => serde_json::to_vec(&JsonlDetailedError {
                schema_version: SCHEMA_VERSION,
                kind: "error",
                command: self.command().to_owned(),
                verdict,
                error: payload,
            }),
            OutputMode::Human => unreachable!("structured mode checked above"),
        };
        let bytes = if let Ok(bytes) = serialized {
            bytes
        } else {
            let fallback = ErrorEnvelope::serialization_failure(self.command());
            if self.mode == OutputMode::Jsonl {
                Self::jsonl_error_bytes(fallback)?
            } else {
                Self::error_bytes(&fallback)?
            }
        };
        let mut output = stdout(self);
        Self::write_bytes(&mut *output, &bytes)?;
        current.terminal = true;
        Ok(())
    }

    pub(crate) fn emit_error(&self, error: ErrorEnvelope) -> anyhow::Result<()> {
        let mut current = state(self);
        let mut stdout = stdout(self);
        self.settle_error_locked(&mut current, &mut *stdout, error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_policy_matrix() {
        for (mode, quiet, structured, progress) in [
            (OutputMode::Human, false, false, true),
            (OutputMode::Human, true, false, false),
            (OutputMode::Json, false, true, false),
            (OutputMode::Jsonl, true, true, false),
        ] {
            let output = Output::new(mode, quiet);
            assert_eq!(output.is_structured(), structured);
            assert_eq!(output.show_progress(), progress);
        }
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
    }

    #[test]
    fn a_prompt_needs_a_terminal_no_no_input_and_an_unstructured_mode() {
        // Asserted against a supplied terminal flag rather than the process's
        // own tty, so the matrix proves the policy instead of proving what the
        // test runner was attached to. The terminal term is the one this
        // predicate gained: a caller that skips `PromptMode` and draws a
        // widget directly can no longer bypass it.
        let human = Output::new(OutputMode::Human, false);
        assert!(human.interactive_on(true));
        assert!(!human.interactive_on(false));
        assert!(!human.clone().with_no_input(true).interactive_on(true));
        assert!(!Output::new(OutputMode::Json, false).interactive_on(true));
        assert!(!Output::new(OutputMode::Jsonl, false).interactive_on(true));
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
    fn jsonl_events_precede_one_terminal_result() {
        #[derive(Clone)]
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
            .with_command("apply")
            .with_writer(SharedWriter(Arc::clone(&bytes)));
        output
            .emit_jsonl_event(serde_json::json!({"sequence": 1}))
            .unwrap();
        output
            .emit_result(ResultVerdict::Ok, serde_json::json!({"ready": true}))
            .unwrap();
        let bytes = bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let lines = std::str::from_utf8(&bytes).unwrap().lines();
        let values = lines
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0]["type"], "event");
        assert_eq!(values[0]["event"]["sequence"], 1);
        assert_eq!(values[1]["type"], "result");
    }

    #[test]
    fn detailed_cancellation_is_one_terminal_error_after_events() {
        #[derive(Clone)]
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
            .with_command("apply")
            .with_writer(SharedWriter(Arc::clone(&bytes)));
        output
            .emit_jsonl_event(serde_json::json!({"sequence": 1}))
            .unwrap();
        output
            .emit_detailed_error(
                ErrorVerdict::Canceled,
                "canceled",
                130,
                "watch canceled after commit",
                "omnifs status --follow --revision 7",
                serde_json::json!({"committed": true, "revision": 7}),
            )
            .unwrap();

        let bytes = bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let values = std::str::from_utf8(&bytes)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0]["type"], "event");
        assert_eq!(values[1]["type"], "error");
        assert_eq!(values[1]["verdict"], "canceled");
        assert_eq!(values[1]["error"]["exit_code"], 130);
        assert_eq!(values[1]["error"]["details"]["committed"], true);
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

        let output = Output::new(OutputMode::Jsonl, false)
            .with_command("status")
            .with_writer(Broken);
        assert!(
            output
                .emit_result(ResultVerdict::Ok, serde_json::json!({}))
                .is_err()
        );

        let error = output
            .emit_result(ResultVerdict::Ok, serde_json::json!({}))
            .unwrap_err();
        assert!(error.to_string().contains("broken stdout"));
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

    // -- PromptMode::resolve: the shared prompt/yes/no-input precedence -----

    #[test]
    fn explicit_value_wins_without_touching_yes_no_input_or_the_prompt() {
        let called = PromptMode::for_test(false, false, true).resolve(
            Some("explicit"),
            || "default",
            "--name",
            || panic!("explicit value must short-circuit before the prompt runs"),
        );
        assert_eq!(called.unwrap(), "explicit");
    }

    #[test]
    fn yes_takes_the_default_without_prompting() {
        let resolved = PromptMode::for_test(true, true, false).resolve(
            None,
            || "default",
            "--name",
            || panic!("--yes must short-circuit before the prompt runs"),
        );
        assert_eq!(resolved.unwrap(), "default");
    }

    #[test]
    fn no_input_bails_naming_the_flag_hint_before_yes_or_the_prompt() {
        let error = PromptMode::for_test(true, false, true)
            .resolve(
                None,
                || "default",
                "--name <name>",
                || panic!("--no-input must bail before the prompt runs"),
            )
            .unwrap_err();
        assert!(error.to_string().contains("--name <name>"));
        assert!(error.to_string().contains("--yes"));
    }

    #[test]
    fn non_interactive_without_no_input_still_bails_naming_the_flag() {
        // A piped stdin with neither --yes nor --no-input is still
        // non-interactive: the bail message is the same shape as --no-input's.
        let error = PromptMode::for_test(false, false, false)
            .resolve(
                None,
                || "default",
                "--name <name>",
                || panic!("a non-interactive run must bail before the prompt runs"),
            )
            .unwrap_err();
        assert!(error.to_string().contains("--name <name>"));
        assert!(error.to_string().contains("terminal"));
    }

    #[test]
    fn yes_as_the_only_automation_flag_is_not_repeated() {
        let error = PromptMode::for_test(false, false, false)
            .resolve(None, || true, "--yes", || Ok(true))
            .unwrap_err();
        assert_eq!(error.to_string(), "this step needs a terminal; pass --yes");
    }

    #[test]
    fn interactive_without_yes_or_no_input_calls_the_prompt() {
        let resolved = PromptMode::for_test(true, false, false).resolve(
            None,
            || "default",
            "--name",
            || Ok("typed"),
        );
        assert_eq!(resolved.unwrap(), "typed");
    }
}
