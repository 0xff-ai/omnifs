//! Unified interactive prompts, built directly on crossterm (spec 2.6).
//!
//! Every prompt draws a transient frame to stderr while raw mode is active,
//! erases it once the answer resolves, and hands the durable one-line echo to
//! [`Output`] so the transcript reads complete after the terminal settles
//! (spec 2.5). Keyboard handling is split in two: `map_*_key` turns a
//! `crossterm::event::KeyEvent` into a domain event, and `*_transition` folds
//! that event into prompt state. Both are pure functions with no I/O, so the
//! state machines are unit-tested directly; `run_prompt_loop` is the only
//! piece that touches the terminal, and it stays untested here (a live
//! terminal loop cannot run under `cargo nextest` without a PTY).

use std::io::{self, IsTerminal, Write as _};

use crossterm::cursor::{Hide, MoveToNextLine, MoveUp, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::queue;
use crossterm::terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode};

use super::output::Output;
use super::report::Row;
use super::style::{self, Glyph, Stream};

/// Marker error returned when an interactive prompt is canceled with Esc or
/// Ctrl-C. The top-level command boundary treats this as a normal exit.
#[derive(Debug)]
pub(crate) struct Canceled;

impl std::fmt::Display for Canceled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("selection canceled")
    }
}

impl std::error::Error for Canceled {}

/// Whether an error represents a canceled interactive prompt.
pub(crate) fn is_canceled(error: &anyhow::Error) -> bool {
    error.downcast_ref::<Canceled>().is_some()
}

/// Whether interactive prompts can safely draw a frame.
///
/// Prompt output is written to stderr, so stdin and stderr must both be
/// terminals. Stdout is intentionally not part of this check: callers may
/// pipe report output while still answering a prompt on the controlling
/// terminal.
pub(crate) fn is_terminal() -> bool {
    io::stdin().is_terminal() && io::stderr().is_terminal()
}

fn not_a_terminal_error() -> anyhow::Error {
    anyhow::anyhow!(
        "this prompt needs a terminal; pass --yes or --no-input with the required flags"
    )
}

fn prompt_error(error: io::Error) -> anyhow::Error {
    match error.kind() {
        io::ErrorKind::Interrupted => anyhow::Error::new(Canceled),
        // A prompt started without a real terminal can still surface as
        // NotConnected from the underlying IO layer rather than failing the
        // `is_terminal()` check up front (e.g. a terminal that disappears
        // mid-prompt). Keep that out of the CLI transcript and point callers
        // at the non-interactive escape hatch instead.
        io::ErrorKind::NotConnected => not_a_terminal_error(),
        _ => anyhow::Error::new(error),
    }
}

/// Print the durable cancellation line. Called only after the transient frame
/// has already been erased and raw mode restored, so this is plain, cooked-mode
/// output like every other line in the transcript.
fn print_canceled(stream: Stream) {
    let _ = writeln!(io::stderr(), "{}", style::dim("canceled", stream));
}

// ---------------------------------------------------------------------------
// Raw-mode terminal loop
// ---------------------------------------------------------------------------

/// Raw mode plus a hidden cursor for the lifetime of one interactive prompt.
/// Both are restored on every exit path, including an early return from a
/// failed terminal call, because teardown lives in `Drop` rather than at each
/// call site.
struct RawTerminal;

impl RawTerminal {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stderr = io::stderr();
        queue!(stderr, Hide)?;
        stderr.flush()?;
        Ok(Self)
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        let mut stderr = io::stderr();
        let _ = queue!(stderr, Show);
        let _ = stderr.flush();
        let _ = disable_raw_mode();
    }
}

/// How one keystroke changes prompt state, decided by a `*_transition`
/// function. `Continue` redraws with the mutated state; `Resolve`/`Cancel`
/// erase the frame and end the loop.
enum LoopControl {
    Continue,
    Resolve,
    Cancel,
}

/// The terminal outcome of a raw-mode prompt loop.
enum Resolution<S> {
    Resolved(S),
    Canceled,
}

/// Block for the next key-press event, ignoring key-release/repeat, resize,
/// mouse, and paste events, none of which a prompt reacts to.
fn next_key_press() -> io::Result<Option<KeyEvent>> {
    match event::read()? {
        Event::Key(key) if key.kind == KeyEventKind::Press => Ok(Some(key)),
        _ => Ok(None),
    }
}

/// Redraw the transient frame in place: move up over the previous frame,
/// clear everything below the cursor, then print the new lines with explicit
/// line breaks (raw mode disables the terminal's own CR-on-LF translation).
fn redraw(drawn: &mut usize, lines: &[String]) -> io::Result<()> {
    let mut out = io::stderr();
    if *drawn > 0 {
        queue!(out, MoveUp(u16::try_from(*drawn).unwrap_or(u16::MAX)))?;
    }
    queue!(out, Clear(ClearType::FromCursorDown))?;
    for line in lines {
        write!(out, "{line}")?;
        queue!(out, MoveToNextLine(1))?;
    }
    out.flush()?;
    *drawn = lines.len();
    Ok(())
}

/// Erase a drawn frame and leave the cursor where the frame started, so the
/// durable echo prints immediately below whatever preceded the prompt.
fn erase(drawn: usize) -> io::Result<()> {
    let mut out = io::stderr();
    if drawn > 0 {
        queue!(out, MoveUp(u16::try_from(drawn).unwrap_or(u16::MAX)))?;
    }
    queue!(out, Clear(ClearType::FromCursorDown))?;
    out.flush()
}

/// The shared raw-mode loop every prompt kind runs: draw the current state,
/// wait for a key, map it to a domain event, and fold it into state until the
/// transition resolves or cancels. `frame`, `map_key`, and `transition` are
/// the only prompt-specific pieces; this function owns the terminal I/O.
fn run_prompt_loop<S, E>(
    mut state: S,
    mut frame: impl FnMut(&S) -> Vec<String>,
    map_key: impl Fn(KeyEvent) -> Option<E>,
    mut transition: impl FnMut(&mut S, E) -> LoopControl,
) -> io::Result<Resolution<S>> {
    let _raw = RawTerminal::enter()?;
    let mut drawn = 0usize;
    loop {
        let lines = frame(&state);
        redraw(&mut drawn, &lines)?;
        let Some(key) = next_key_press()? else {
            continue;
        };
        let Some(event) = map_key(key) else {
            continue;
        };
        match transition(&mut state, event) {
            LoopControl::Continue => {},
            LoopControl::Resolve => {
                erase(drawn)?;
                return Ok(Resolution::Resolved(state));
            },
            LoopControl::Cancel => {
                erase(drawn)?;
                return Ok(Resolution::Canceled);
            },
        }
    }
}

fn is_ctrl_c(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)
}

// ---------------------------------------------------------------------------
// Text and password: single-line editing
// ---------------------------------------------------------------------------

struct LineState {
    buffer: String,
}

#[derive(Clone, Copy)]
enum LineEvent {
    Char(char),
    Backspace,
    Confirm,
    Cancel,
}

fn map_line_key(key: KeyEvent) -> Option<LineEvent> {
    if is_ctrl_c(key) {
        return Some(LineEvent::Cancel);
    }
    match key.code {
        KeyCode::Backspace => Some(LineEvent::Backspace),
        KeyCode::Enter => Some(LineEvent::Confirm),
        KeyCode::Esc => Some(LineEvent::Cancel),
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            Some(LineEvent::Char(c))
        },
        _ => None,
    }
}

fn line_transition(state: &mut LineState, event: LineEvent) -> LoopControl {
    match event {
        LineEvent::Char(c) => {
            state.buffer.push(c);
            LoopControl::Continue
        },
        LineEvent::Backspace => {
            state.buffer.pop();
            LoopControl::Continue
        },
        LineEvent::Confirm => LoopControl::Resolve,
        LineEvent::Cancel => LoopControl::Cancel,
    }
}

fn line_frame(prefix: &str, buffer: &str, mask: bool, stream: Stream) -> Vec<String> {
    let shown = if mask {
        "*".repeat(buffer.chars().count())
    } else {
        buffer.to_owned()
    };
    vec![format!("{prefix} {}", style::accent(&shown, stream))]
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

    pub(crate) fn ask_with_output(self, output: &Output) -> anyhow::Result<String> {
        output.ensure_prompt_allowed()?;
        if !is_terminal() {
            return Err(not_a_terminal_error());
        }
        let stream = Stream::Stderr;
        let question = self.question;
        let default = self.default;
        let default_hint = default
            .as_deref()
            .map(|value| format!("  {}", style::dim(format!("[{value}]"), stream)))
            .unwrap_or_default();
        let prefix = format!("{}{default_hint}", style::accentuate(&question, stream));
        let resolution = run_prompt_loop(
            LineState {
                buffer: String::new(),
            },
            |state| line_frame(&prefix, &state.buffer, false, stream),
            map_line_key,
            line_transition,
        )
        .map_err(prompt_error)?;
        match resolution {
            Resolution::Resolved(state) => {
                let answer = if state.buffer.is_empty() {
                    default.unwrap_or_default()
                } else {
                    state.buffer
                };
                output.answer(&question, &answer);
                Ok(answer)
            },
            Resolution::Canceled => {
                print_canceled(stream);
                Err(Canceled.into())
            },
        }
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

    pub(crate) fn ask_with_output(self, output: &Output) -> anyhow::Result<String> {
        output.ensure_prompt_allowed()?;
        if !is_terminal() {
            return Err(not_a_terminal_error());
        }
        let stream = Stream::Stderr;
        let question = self.question;
        let prefix = style::accentuate(&question, stream);
        let resolution = run_prompt_loop(
            LineState {
                buffer: String::new(),
            },
            |state| line_frame(&prefix, &state.buffer, true, stream),
            map_line_key,
            line_transition,
        )
        .map_err(prompt_error)?;
        match resolution {
            Resolution::Resolved(state) => {
                output.answer(&question, "answered");
                Ok(state.buffer)
            },
            Resolution::Canceled => {
                print_canceled(stream);
                Err(Canceled.into())
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Confirm
// ---------------------------------------------------------------------------

struct ConfirmState {
    value: bool,
}

#[derive(Clone, Copy)]
enum ConfirmEvent {
    Toggle,
    Yes,
    No,
    Confirm,
    Cancel,
}

fn map_confirm_key(key: KeyEvent) -> Option<ConfirmEvent> {
    if is_ctrl_c(key) {
        return Some(ConfirmEvent::Cancel);
    }
    match key.code {
        KeyCode::Left | KeyCode::Right | KeyCode::Tab => Some(ConfirmEvent::Toggle),
        KeyCode::Char('y' | 'Y') => Some(ConfirmEvent::Yes),
        KeyCode::Char('n' | 'N') => Some(ConfirmEvent::No),
        KeyCode::Enter => Some(ConfirmEvent::Confirm),
        KeyCode::Esc => Some(ConfirmEvent::Cancel),
        _ => None,
    }
}

fn confirm_transition(state: &mut ConfirmState, event: ConfirmEvent) -> LoopControl {
    match event {
        ConfirmEvent::Toggle => {
            state.value = !state.value;
            LoopControl::Continue
        },
        ConfirmEvent::Yes => {
            state.value = true;
            LoopControl::Resolve
        },
        ConfirmEvent::No => {
            state.value = false;
            LoopControl::Resolve
        },
        ConfirmEvent::Confirm => LoopControl::Resolve,
        ConfirmEvent::Cancel => LoopControl::Cancel,
    }
}

fn confirm_frame(question_line: &str, value: bool, stream: Stream) -> Vec<String> {
    let choice = if value { "yes" } else { "no" };
    vec![format!("{question_line} {}", style::accent(choice, stream))]
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

    pub(crate) fn ask_with_output(self, output: &Output) -> anyhow::Result<bool> {
        output.ensure_prompt_allowed()?;
        if !is_terminal() {
            return Err(not_a_terminal_error());
        }
        let stream = Stream::Stderr;
        let question = self.question;
        let question_line = format!(
            "{} {}",
            style::accent("?", stream),
            style::accentuate(&question, stream)
        );
        let resolution = run_prompt_loop(
            ConfirmState {
                value: self.default,
            },
            |state| confirm_frame(&question_line, state.value, stream),
            map_confirm_key,
            confirm_transition,
        )
        .map_err(prompt_error)?;
        match resolution {
            Resolution::Resolved(state) => {
                let answer = if state.value { "yes" } else { "no" };
                output.answer(&question, answer);
                Ok(state.value)
            },
            Resolution::Canceled => {
                print_canceled(stream);
                Err(Canceled.into())
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Select and multi-select
// ---------------------------------------------------------------------------

/// One picker choice: `value` is what the caller gets back, `label` is what
/// is drawn in the option row, and `detail` is the full-sentence description
/// shown in the panel below the list while the row is highlighted (empty for
/// options with nothing more to say than their label).
struct SelectItem<T> {
    value: T,
    label: String,
    detail: Vec<String>,
}

fn item_from_value<T: std::fmt::Display>(value: T) -> SelectItem<T> {
    let label = value.to_string();
    SelectItem {
        value,
        label,
        detail: Vec::new(),
    }
}

fn item_from_option<T>(value: T, label: String, hint: String) -> SelectItem<T> {
    let detail = if hint.is_empty() {
        Vec::new()
    } else {
        vec![hint]
    };
    SelectItem {
        value,
        label,
        detail,
    }
}

/// Which way a browse key moves the picker cursor.
#[derive(Clone, Copy)]
enum Direction {
    Up,
    Down,
}

/// Move a picker cursor by one step with wraparound, the shared browse
/// behavior for both select and multi-select.
fn move_cursor(cursor: &mut usize, len: usize, direction: Direction) {
    if len == 0 {
        return;
    }
    *cursor = match direction {
        Direction::Up => cursor.checked_sub(1).unwrap_or(len - 1),
        Direction::Down => (*cursor + 1) % len,
    };
}

/// The left-bordered, dimmed detail panel under a picker's option list.
/// Empty when the highlighted option carries no detail, so a plain list of
/// bare names never grows a panel with nothing in it.
fn detail_panel(detail: &[String], stream: Stream) -> Vec<String> {
    detail
        .iter()
        .map(|line| {
            format!(
                "{} {}",
                style::dim("\u{2502}", stream),
                style::dim(line, stream)
            )
        })
        .collect()
}

#[derive(Clone, Copy)]
enum PickerEvent {
    Up,
    Down,
    Toggle,
    Confirm,
    Cancel,
}

fn map_picker_key(key: KeyEvent) -> Option<PickerEvent> {
    if is_ctrl_c(key) {
        return Some(PickerEvent::Cancel);
    }
    match key.code {
        KeyCode::Up => Some(PickerEvent::Up),
        KeyCode::Down => Some(PickerEvent::Down),
        KeyCode::Char(' ') => Some(PickerEvent::Toggle),
        KeyCode::Enter => Some(PickerEvent::Confirm),
        KeyCode::Esc => Some(PickerEvent::Cancel),
        _ => None,
    }
}

fn select_transition(cursor: &mut usize, len: usize, event: PickerEvent) -> LoopControl {
    match event {
        PickerEvent::Up => {
            move_cursor(cursor, len, Direction::Up);
            LoopControl::Continue
        },
        PickerEvent::Down => {
            move_cursor(cursor, len, Direction::Down);
            LoopControl::Continue
        },
        PickerEvent::Confirm => LoopControl::Resolve,
        PickerEvent::Cancel => LoopControl::Cancel,
        PickerEvent::Toggle => LoopControl::Continue,
    }
}

fn select_frame<T>(
    hint_line: &str,
    items: &[SelectItem<T>],
    cursor: usize,
    stream: Stream,
) -> Vec<String> {
    let mut lines = vec![hint_line.to_owned()];
    for (index, item) in items.iter().enumerate() {
        let selected = index == cursor;
        let marker = if selected {
            style::accent("\u{203a}", stream)
        } else {
            " ".to_owned()
        };
        let label = if selected {
            style::accent(&item.label, stream)
        } else {
            item.label.clone()
        };
        lines.push(format!("{marker} {label}"));
    }
    if let Some(item) = items.get(cursor) {
        lines.extend(detail_panel(&item.detail, stream));
    }
    lines
}

pub(crate) struct Select<T> {
    question: String,
    key: Option<String>,
    items: Vec<SelectItem<T>>,
}

impl<T: Clone + Eq + std::fmt::Display> Select<T> {
    pub(crate) fn new(question: impl Into<String>) -> Self {
        Self {
            question: question.into(),
            key: None,
            items: Vec::new(),
        }
    }

    /// Echo the resolved answer as a `render.rs` ledger row keyed by `key`
    /// instead of the default question-plus-answer sentence. No current call
    /// site needs this; it exists so a later slice can key select echoes the
    /// same way multi-select already must.
    #[allow(dead_code)]
    pub(crate) fn key(mut self, key: impl Into<String>) -> Self {
        self.key = Some(key.into());
        self
    }

    pub(crate) fn items(mut self, items: impl IntoIterator<Item = T>) -> Self {
        self.items.extend(items.into_iter().map(item_from_value));
        self
    }

    /// Add explicit `(value, label, hint)` choices when a value's display text
    /// is not the right prompt label. `hint` becomes the detail-panel text
    /// shown while the option is highlighted.
    pub(crate) fn options(mut self, items: impl IntoIterator<Item = (T, String, String)>) -> Self {
        self.items.extend(
            items
                .into_iter()
                .map(|(value, label, hint)| item_from_option(value, label, hint)),
        );
        self
    }

    pub(crate) fn ask_with_output(self, output: &Output) -> anyhow::Result<T> {
        output.ensure_prompt_allowed()?;
        if !is_terminal() {
            return Err(not_a_terminal_error());
        }
        if self.items.is_empty() {
            anyhow::bail!("no choices to select from");
        }
        let stream = Stream::Stderr;
        let question = self.question;
        let key = self.key;
        let items = self.items;
        let len = items.len();
        let hint_line = format!(
            "{}  {}",
            style::accentuate(&question, stream),
            style::dim(
                "(\u{2191}\u{2193} browse, enter select, esc cancel)",
                stream
            )
        );
        let resolution = run_prompt_loop(
            0usize,
            |cursor| select_frame(&hint_line, &items, *cursor, stream),
            map_picker_key,
            |cursor, event| select_transition(cursor, len, event),
        )
        .map_err(prompt_error)?;
        match resolution {
            Resolution::Resolved(cursor) => {
                let chosen = &items[cursor];
                match &key {
                    Some(key) => output.row(&Row::new(Glyph::Done, key.clone(), &chosen.label)),
                    None => output.answer(&question, &chosen.label),
                }
                Ok(chosen.value.clone())
            },
            Resolution::Canceled => {
                print_canceled(stream);
                Err(Canceled.into())
            },
        }
    }
}

struct MultiSelectState {
    cursor: usize,
    checked: Vec<bool>,
}

fn multi_select_transition(
    state: &mut MultiSelectState,
    len: usize,
    event: PickerEvent,
) -> LoopControl {
    match event {
        PickerEvent::Up => {
            move_cursor(&mut state.cursor, len, Direction::Up);
            LoopControl::Continue
        },
        PickerEvent::Down => {
            move_cursor(&mut state.cursor, len, Direction::Down);
            LoopControl::Continue
        },
        PickerEvent::Toggle => {
            if let Some(checked) = state.checked.get_mut(state.cursor) {
                *checked = !*checked;
            }
            LoopControl::Continue
        },
        PickerEvent::Confirm => LoopControl::Resolve,
        PickerEvent::Cancel => LoopControl::Cancel,
    }
}

fn multi_select_frame<T>(
    hint_line: &str,
    items: &[SelectItem<T>],
    state: &MultiSelectState,
    stream: Stream,
) -> Vec<String> {
    let mut lines = vec![hint_line.to_owned()];
    for (index, item) in items.iter().enumerate() {
        let selected = index == state.cursor;
        let marker = if selected {
            style::accent("\u{203a}", stream)
        } else {
            " ".to_owned()
        };
        let checkbox = if state.checked[index] { "[x]" } else { "[ ]" };
        let label = if selected {
            style::accent(&item.label, stream)
        } else {
            item.label.clone()
        };
        lines.push(format!("{marker} {checkbox} {label}"));
    }
    if let Some(item) = items.get(state.cursor) {
        lines.extend(detail_panel(&item.detail, stream));
    }
    lines
}

/// A multi-select prompt. Unlike [`Select`], its echo is always a ledger row
/// (spec 2.6's `✓ services    github, dns` example), so `key` is required at
/// construction rather than optional.
#[allow(dead_code)] // wired up by a later slice (S3/S4) once a command needs it
pub(crate) struct MultiSelect<T> {
    question: String,
    key: String,
    items: Vec<SelectItem<T>>,
}

#[allow(dead_code)] // wired up by a later slice (S3/S4) once a command needs it
impl<T: Clone + Eq + std::fmt::Display> MultiSelect<T> {
    pub(crate) fn new(question: impl Into<String>, key: impl Into<String>) -> Self {
        Self {
            question: question.into(),
            key: key.into(),
            items: Vec::new(),
        }
    }

    pub(crate) fn items(mut self, items: impl IntoIterator<Item = T>) -> Self {
        self.items.extend(items.into_iter().map(item_from_value));
        self
    }

    pub(crate) fn options(mut self, items: impl IntoIterator<Item = (T, String, String)>) -> Self {
        self.items.extend(
            items
                .into_iter()
                .map(|(value, label, hint)| item_from_option(value, label, hint)),
        );
        self
    }

    pub(crate) fn ask_with_output(self, output: &Output) -> anyhow::Result<Vec<T>> {
        output.ensure_prompt_allowed()?;
        if !is_terminal() {
            return Err(not_a_terminal_error());
        }
        if self.items.is_empty() {
            anyhow::bail!("no choices to select from");
        }
        let stream = Stream::Stderr;
        let key = self.key;
        let items = self.items;
        let len = items.len();
        let hint_line = format!(
            "{}  {}",
            style::accentuate(&self.question, stream),
            style::dim(
                "(\u{2191}\u{2193} browse, space toggle, enter confirm, esc cancel)",
                stream
            )
        );
        let resolution = run_prompt_loop(
            MultiSelectState {
                cursor: 0,
                checked: vec![false; len],
            },
            |state| multi_select_frame(&hint_line, &items, state, stream),
            map_picker_key,
            |state, event| multi_select_transition(state, len, event),
        )
        .map_err(prompt_error)?;
        match resolution {
            Resolution::Resolved(state) => {
                let chosen: Vec<T> = items
                    .iter()
                    .zip(&state.checked)
                    .filter(|(_, checked)| **checked)
                    .map(|(item, _)| item.value.clone())
                    .collect();
                let labels: Vec<&str> = items
                    .iter()
                    .zip(&state.checked)
                    .filter(|(_, checked)| **checked)
                    .map(|(item, _)| item.label.as_str())
                    .collect();
                let value = if labels.is_empty() {
                    "none".to_owned()
                } else {
                    labels.join(", ")
                };
                output.row(&Row::new(Glyph::Done, key, value));
                Ok(chosen)
            },
            Resolution::Canceled => {
                print_canceled(stream);
                Err(Canceled.into())
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- io error classification --------------------------------------

    #[test]
    fn interrupted_is_shared_cancel() {
        let error = prompt_error(io::ErrorKind::Interrupted.into());
        assert!(is_canceled(&error));
    }

    #[test]
    fn other_io_errors_are_not_cancel() {
        let error = prompt_error(io::ErrorKind::NotConnected.into());
        assert!(!is_canceled(&error));
        assert!(error.to_string().contains("pass --yes or --no-input"));
    }

    #[test]
    fn structured_prompt_policy_fails_before_display() {
        let output = Output::new(super::super::output::OutputMode::Json, false);
        let error = Confirm::new("Proceed?")
            .ask_with_output(&output)
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("interactive input is unavailable")
        );
    }

    // -- line editor transitions ----------------------------------------

    #[test]
    fn line_editor_appends_and_backspaces() {
        let mut state = LineState {
            buffer: String::new(),
        };
        for event in [LineEvent::Char('h'), LineEvent::Char('i')] {
            assert!(matches!(
                line_transition(&mut state, event),
                LoopControl::Continue
            ));
        }
        assert_eq!(state.buffer, "hi");
        assert!(matches!(
            line_transition(&mut state, LineEvent::Backspace),
            LoopControl::Continue
        ));
        assert_eq!(state.buffer, "h");
        assert!(matches!(
            line_transition(&mut state, LineEvent::Confirm),
            LoopControl::Resolve
        ));
        assert!(matches!(
            line_transition(&mut state, LineEvent::Cancel),
            LoopControl::Cancel
        ));
    }

    #[test]
    fn line_key_mapping_recognizes_ctrl_c_and_plain_chars() {
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(map_line_key(ctrl_c), Some(LineEvent::Cancel)));
        let letter = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        assert!(matches!(map_line_key(letter), Some(LineEvent::Char('x'))));
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(map_line_key(enter), Some(LineEvent::Confirm)));
    }

    // -- confirm transitions ---------------------------------------------

    #[test]
    fn confirm_toggle_flips_the_pending_value_without_resolving() {
        let mut state = ConfirmState { value: false };
        assert!(matches!(
            confirm_transition(&mut state, ConfirmEvent::Toggle),
            LoopControl::Continue
        ));
        assert!(state.value);
    }

    #[test]
    fn confirm_yes_and_no_resolve_immediately_regardless_of_pending_value() {
        let mut state = ConfirmState { value: false };
        assert!(matches!(
            confirm_transition(&mut state, ConfirmEvent::Yes),
            LoopControl::Resolve
        ));
        assert!(state.value);

        let mut state = ConfirmState { value: true };
        assert!(matches!(
            confirm_transition(&mut state, ConfirmEvent::No),
            LoopControl::Resolve
        ));
        assert!(!state.value);
    }

    #[test]
    fn confirm_enter_resolves_the_current_default() {
        let mut state = ConfirmState { value: true };
        assert!(matches!(
            confirm_transition(&mut state, ConfirmEvent::Confirm),
            LoopControl::Resolve
        ));
        assert!(state.value);
    }

    #[test]
    fn confirm_cancel_never_resolves() {
        let mut state = ConfirmState { value: true };
        assert!(matches!(
            confirm_transition(&mut state, ConfirmEvent::Cancel),
            LoopControl::Cancel
        ));
    }

    #[test]
    fn confirm_key_mapping_covers_yes_no_and_ctrl_c() {
        assert!(matches!(
            map_confirm_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
            Some(ConfirmEvent::Yes)
        ));
        assert!(matches!(
            map_confirm_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)),
            Some(ConfirmEvent::No)
        ));
        assert!(matches!(
            map_confirm_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(ConfirmEvent::Cancel)
        ));
        assert!(matches!(
            map_confirm_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            Some(ConfirmEvent::Toggle)
        ));
    }

    // -- select / multi-select cursor and toggle transitions -------------

    #[test]
    fn move_cursor_wraps_in_both_directions() {
        let mut cursor = 0;
        move_cursor(&mut cursor, 3, Direction::Up);
        assert_eq!(cursor, 2, "moving up from the first row wraps to the last");
        move_cursor(&mut cursor, 3, Direction::Down);
        assert_eq!(cursor, 0);
        move_cursor(&mut cursor, 3, Direction::Down);
        assert_eq!(cursor, 1);
    }

    #[test]
    fn move_cursor_on_an_empty_list_is_a_no_op() {
        let mut cursor = 0;
        move_cursor(&mut cursor, 0, Direction::Down);
        assert_eq!(cursor, 0);
    }

    #[test]
    fn select_transition_browses_and_resolves() {
        let mut cursor = 0usize;
        assert!(matches!(
            select_transition(&mut cursor, 3, PickerEvent::Down),
            LoopControl::Continue
        ));
        assert_eq!(cursor, 1);
        assert!(matches!(
            select_transition(&mut cursor, 3, PickerEvent::Confirm),
            LoopControl::Resolve
        ));
        assert!(matches!(
            select_transition(&mut cursor, 3, PickerEvent::Cancel),
            LoopControl::Cancel
        ));
        // Toggle has no meaning for a single-select and never resolves it.
        assert!(matches!(
            select_transition(&mut cursor, 3, PickerEvent::Toggle),
            LoopControl::Continue
        ));
    }

    #[test]
    fn multi_select_transition_toggles_the_highlighted_row_only() {
        let mut state = MultiSelectState {
            cursor: 0,
            checked: vec![false, false, false],
        };
        multi_select_transition(&mut state, 3, PickerEvent::Down);
        assert_eq!(state.cursor, 1);
        assert!(matches!(
            multi_select_transition(&mut state, 3, PickerEvent::Toggle),
            LoopControl::Continue
        ));
        assert_eq!(state.checked, [false, true, false]);
        // Toggling again unchecks it.
        multi_select_transition(&mut state, 3, PickerEvent::Toggle);
        assert_eq!(state.checked, [false, false, false]);
    }

    #[test]
    fn multi_select_confirm_resolves_with_whatever_is_checked() {
        let mut state = MultiSelectState {
            cursor: 2,
            checked: vec![true, false, true],
        };
        assert!(matches!(
            multi_select_transition(&mut state, 3, PickerEvent::Confirm),
            LoopControl::Resolve
        ));
        assert_eq!(state.checked, [true, false, true]);
    }

    #[test]
    fn multi_select_cancel_never_resolves() {
        let mut state = MultiSelectState {
            cursor: 0,
            checked: vec![true],
        };
        assert!(matches!(
            multi_select_transition(&mut state, 1, PickerEvent::Cancel),
            LoopControl::Cancel
        ));
    }

    #[test]
    fn picker_key_mapping_covers_browse_toggle_and_cancel() {
        assert!(matches!(
            map_picker_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            Some(PickerEvent::Up)
        ));
        assert!(matches!(
            map_picker_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            Some(PickerEvent::Down)
        ));
        assert!(matches!(
            map_picker_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)),
            Some(PickerEvent::Toggle)
        ));
        assert!(matches!(
            map_picker_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Some(PickerEvent::Cancel)
        ));
        assert!(matches!(
            map_picker_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(PickerEvent::Cancel)
        ));
        assert!(map_picker_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE)).is_none());
    }

    // -- frame rendering: detail panel and echo shape ---------------------

    #[test]
    fn detail_panel_is_empty_for_options_without_a_description() {
        assert!(detail_panel(&[], Stream::Stderr).is_empty());
    }

    #[test]
    fn detail_panel_renders_every_line_with_a_left_border() {
        let lines = detail_panel(
            &["calls api.github.com".to_owned(), "static token".to_owned()],
            Stream::Stderr,
        );
        assert_eq!(lines.len(), 2);
        for line in &lines {
            assert!(strip_ansi_local(line).starts_with('\u{2502}'), "{line:?}");
        }
    }

    #[test]
    fn select_frame_marks_only_the_cursor_row() {
        let items = vec![
            item_from_value("alpha".to_owned()),
            item_from_value("beta".to_owned()),
        ];
        let lines = select_frame("Pick one", &items, 1, Stream::Stderr);
        // hint line + one row per item, no panel (no detail on plain items).
        assert_eq!(lines.len(), 1 + items.len());
        assert!(strip_ansi_local(&lines[1]).starts_with(' '));
        assert!(strip_ansi_local(&lines[2]).starts_with('\u{203a}'));
    }

    #[test]
    fn multi_select_frame_shows_checkbox_state_per_row() {
        let items = vec![
            item_from_value("github".to_owned()),
            item_from_value("dns".to_owned()),
        ];
        let state = MultiSelectState {
            cursor: 0,
            checked: vec![true, false],
        };
        let lines = multi_select_frame("Pick services", &items, &state, Stream::Stderr);
        assert!(strip_ansi_local(&lines[1]).contains("[x]"));
        assert!(strip_ansi_local(&lines[2]).contains("[ ]"));
    }

    fn strip_ansi_local(text: &str) -> String {
        crate::ui::strip_ansi(text)
    }
}
