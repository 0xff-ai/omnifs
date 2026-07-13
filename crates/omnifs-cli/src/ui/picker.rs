#![allow(clippy::disallowed_macros)] // ui/ owns terminal output
//! Custom inline provider picker with a details side panel.
//!
//! The general-purpose prompt does not render a details panel, so this component draws
//! its own list plus an expandable panel using `crossterm` for raw-mode keys and
//! cursor control. It renders inline (no alternate screen): each keystroke
//! redraws in place, and on finish the block is cleared and replaced by one
//! answered line so scrollback matches the rail prompts around it.
//!
//! Only the pure parts (row building, tag derivation, truncation, `default_on`,
//! panel assembly) are unit-tested; raw-mode rendering is exercised live.

use std::collections::BTreeMap;
use std::io::{IsTerminal as _, Write as _};

use anyhow::anyhow;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    queue,
    terminal::{self, Clear, ClearType},
};
use omnifs_caps::AccessNeed;
use omnifs_workspace::authn::AuthScheme;
use omnifs_workspace::provider::{Provider, ProviderManifest};

use crate::ui::style;

/// Marker error returned by [`select`]/[`multiselect`] when the user cancels
/// (Esc or Ctrl-C). Callers that treat cancel as a normal exit downcast via
/// [`is_canceled`]; the default is to propagate and abort like any other error.
#[derive(Debug)]
pub(crate) struct Canceled;

impl std::fmt::Display for Canceled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("selection canceled")
    }
}

impl std::error::Error for Canceled {}

/// Whether an error is a picker cancellation.
pub(crate) fn is_canceled(error: &anyhow::Error) -> bool {
    error.downcast_ref::<Canceled>().is_some()
}

/// Capability tag derived from a manifest's access needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CapTag {
    Net,
    Git,
    Fs,
    Sock,
}

impl CapTag {
    fn label(self) -> &'static str {
        match self {
            CapTag::Net => "net",
            CapTag::Git => "git",
            CapTag::Fs => "fs",
            CapTag::Sock => "sock",
        }
    }

    /// Stable render order: Net, Git, Fs, Sock.
    fn order(self) -> u8 {
        match self {
            CapTag::Net => 0,
            CapTag::Git => 1,
            CapTag::Fs => 2,
            CapTag::Sock => 3,
        }
    }
}

/// Auth tag derived from a manifest's default auth scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthTag {
    SignIn,
    ApiKey,
}

impl AuthTag {
    fn label(self) -> &'static str {
        match self {
            AuthTag::SignIn => "sign-in",
            AuthTag::ApiKey => "API key",
        }
    }
}

/// One pre-rendered detail panel line and its color role.
#[derive(Debug, Clone)]
pub(crate) struct PanelLine {
    pub(crate) text: String,
    pub(crate) role: PanelRole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PanelRole {
    Head,
    Plain,
    Section,
    Dim,
}

#[derive(Debug, Clone)]
pub(crate) struct Detail {
    pub(crate) lines: Vec<PanelLine>,
}

/// One selectable provider row.
#[derive(Debug, Clone)]
pub(crate) struct PickerRow {
    pub(crate) id: String,
    pub(crate) summary: String,
    pub(crate) cap_tags: Vec<CapTag>,
    pub(crate) auth_tag: Option<AuthTag>,
    pub(crate) default_on: bool,
    pub(crate) detail: Detail,
}

/// Derive the deduped, ordered capability tags for a manifest.
pub(crate) fn cap_tags(manifest: &ProviderManifest) -> Vec<CapTag> {
    let mut tags: Vec<CapTag> = Vec::new();
    for entry in &manifest.capabilities {
        let tag = match entry {
            AccessNeed::Domain { .. } => CapTag::Net,
            AccessNeed::GitRepo { .. } => CapTag::Git,
            AccessNeed::PreopenedPath { .. } => CapTag::Fs,
            AccessNeed::UnixSocket { .. } => CapTag::Sock,
        };
        if !tags.contains(&tag) {
            tags.push(tag);
        }
    }
    tags.sort_by_key(|tag| tag.order());
    tags
}

/// Derive the auth tag from the manifest's default scheme.
pub(crate) fn auth_tag(manifest: &ProviderManifest) -> Option<AuthTag> {
    let (_, scheme) = manifest.default_scheme()?;
    match scheme {
        AuthScheme::Oauth(_) => Some(AuthTag::SignIn),
        AuthScheme::StaticToken(_) => Some(AuthTag::ApiKey),
        AuthScheme::None => None,
    }
}

/// Whether a provider defaults ON in the picker: it can complete without the
/// user producing out-of-band state, and it does not require an interactive
/// config prompt.
pub(crate) fn default_on(manifest: &ProviderManifest) -> bool {
    let no_prompt = manifest
        .config
        .as_ref()
        .is_none_or(|config| !config.requires_prompt());
    if !no_prompt {
        return false;
    }
    if manifest.auth.is_none() {
        return true;
    }
    let default_is_oauth = matches!(manifest.default_scheme(), Some((_, AuthScheme::Oauth(_))));
    if default_is_oauth {
        return true;
    }
    let ambient = crate::commands::mount::detect::detect(manifest.wasm_auth_manifest().as_ref());
    !ambient.is_empty()
}

/// Build the panel detail for one provider.
fn build_detail(manifest: &ProviderManifest, summary: &str) -> Detail {
    let mut lines = Vec::new();
    // Header: `id, display_name` with the id bold (no em-dash per house style).
    lines.push(PanelLine {
        text: format!("{}, {}", style::bold(&manifest.id), manifest.display_name),
        role: PanelRole::Head,
    });
    lines.push(PanelLine {
        text: summary.to_string(),
        role: PanelRole::Plain,
    });
    // Auth line.
    let auth_line = match manifest.default_scheme() {
        Some((_, AuthScheme::Oauth(_))) => "sign-in with your browser".to_string(),
        Some((_, AuthScheme::StaticToken(scheme))) => {
            let mut line = "needs an API key".to_string();
            if let Some(url) = &scheme.creation_url {
                line.push_str("  ");
                line.push_str(url);
            }
            line
        },
        _ => "no credentials needed".to_string(),
    };
    lines.push(PanelLine {
        text: auth_line,
        role: PanelRole::Plain,
    });

    // needs / limits two-column rows, left column padded to the longest label.
    let rows = crate::capability::detail_lines(manifest);
    let needs_count = crate::capability::needs_row_count(manifest);
    let label_width = rows
        .iter()
        .map(|(left, _)| left.chars().count())
        .max()
        .unwrap_or(0);
    if needs_count > 0 {
        lines.push(PanelLine {
            text: "needs".to_string(),
            role: PanelRole::Section,
        });
        for (left, right) in rows.iter().take(needs_count) {
            lines.push(two_col(left, right, label_width));
        }
    }
    if rows.len() > needs_count {
        lines.push(PanelLine {
            text: "limits".to_string(),
            role: PanelRole::Section,
        });
        for (left, right) in rows.iter().skip(needs_count) {
            lines.push(two_col(left, right, label_width));
        }
    }

    // notes: provider auth guidance for the default scheme, if any.
    if let Some((key, _)) = manifest.default_scheme() {
        let key = key.to_string();
        if let Some(auth) = &manifest.auth {
            let guidance = auth.guidance_for(&key);
            let mut notes: Vec<String> = Vec::new();
            if let Some(summary) = &guidance.summary {
                notes.push(summary.clone());
            }
            notes.extend(guidance.setup_steps.iter().cloned());
            if let Some(url) = &guidance.docs_url {
                notes.push(url.clone());
            }
            if !notes.is_empty() {
                lines.push(PanelLine {
                    text: "notes".to_string(),
                    role: PanelRole::Section,
                });
                for note in notes {
                    lines.push(PanelLine {
                        text: format!("  {note}"),
                        role: PanelRole::Dim,
                    });
                }
            }
        }
    }

    Detail { lines }
}

fn two_col(left: &str, right: &str, label_width: usize) -> PanelLine {
    let pad = label_width.saturating_sub(left.chars().count());
    PanelLine {
        text: format!("  {left}{:pad$}  {right}", "", pad = pad),
        role: PanelRole::Dim,
    }
}

/// Build the picker rows for the installed providers, skipping any already
/// configured. Order: default-on first, then alphabetical within each group.
pub(crate) fn build_rows(
    installed: &[(Provider, ProviderManifest)],
    configured: &BTreeMap<String, String>,
) -> Vec<PickerRow> {
    let mut rows: Vec<PickerRow> = installed
        .iter()
        .filter(|(provider, _)| !configured.contains_key(provider.meta.name.as_str()))
        .map(|(provider, manifest)| {
            let summary = manifest
                .description
                .clone()
                .unwrap_or_else(|| manifest.display_name.clone());
            // The returned id is the installable name that `mount add` consumes, which
            // usually equals the manifest id but can differ (test fixtures).
            PickerRow {
                id: provider.meta.name.to_string(),
                summary: summary.clone(),
                cap_tags: cap_tags(manifest),
                auth_tag: auth_tag(manifest),
                default_on: default_on(manifest),
                detail: build_detail(manifest, &summary),
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        b.default_on
            .cmp(&a.default_on)
            .then_with(|| a.id.cmp(&b.id))
    });
    rows
}

fn tags_plain(row: &PickerRow) -> String {
    row.cap_tags
        .iter()
        .map(|tag| tag.label())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Render one list row to a colored string of exactly `width` visible columns.
/// The single-select variant (`multi = false`) renders no checkbox glyph.
fn render_row(
    row: &PickerRow,
    id_width: usize,
    highlighted: bool,
    selected: bool,
    multi: bool,
    width: usize,
) -> String {
    let arrow = if highlighted {
        style::accent("› ")
    } else {
        "  ".to_string()
    };
    let checkbox = if !multi {
        " ".to_string()
    } else if selected {
        style::accent("◉")
    } else {
        style::dim("◯")
    };
    let id_pad = id_width.saturating_sub(row.id.chars().count());
    let id_field = format!("{}{:pad$}", row.id, "", pad = id_pad);

    // Visible width consumed before the summary: arrow(2) + checkbox(1) + space
    // + id_field + space.
    let prefix_visible = 2 + 1 + 1 + id_width + 1;

    let tags = tags_plain(row);
    let auth = row.auth_tag.map(|tag| tag.label().to_string());
    let right_visible = tags.chars().count()
        + auth.as_ref().map_or(0, |auth| {
            (if tags.is_empty() { 0 } else { 3 }) + auth.chars().count()
        });

    let avail = width.saturating_sub(prefix_visible);
    let summary_max = avail.saturating_sub(right_visible + 1);
    let summary = crate::ui::truncate(&row.summary, summary_max);
    let summary_visible = summary.chars().count();
    let gap = avail.saturating_sub(summary_visible + right_visible);

    let mut right = String::new();
    if !tags.is_empty() {
        right.push_str(&style::dim(&tags));
    }
    if let Some(auth) = &auth {
        if !tags.is_empty() {
            right.push_str("   ");
        }
        right.push_str(&style::warn(auth));
    }

    format!(
        "{arrow}{checkbox} {id_field} {summary}{:gap$}{right}",
        "",
        gap = gap
    )
}

/// Color a panel line by its role.
fn render_panel_line(line: &PanelLine) -> String {
    match line.role {
        PanelRole::Head | PanelRole::Plain => line.text.clone(),
        PanelRole::Section => style::bold(&line.text),
        PanelRole::Dim => style::dim(&line.text),
    }
}

/// Interactive selection over `rows`. `multi` toggles checkboxes; otherwise
/// `enter` selects the highlighted row.
struct Picker {
    question: String,
    rows: Vec<PickerRow>,
    checked: Vec<bool>,
    cursor: usize,
    multi: bool,
    panel_open: bool,
    last_height: u16,
    /// Whether to keep the color our `style` helpers emit. The picker writes
    /// frames straight to stderr, bypassing `anstream`, so it must honor
    /// `NO_COLOR` and non-TTY itself instead of relying on anstream's stripping.
    color: bool,
}

/// Color is on only for a color-capable TTY with `NO_COLOR` unset. Resolved once
/// at picker entry and applied to every emitted frame.
fn color_enabled() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::stderr().is_terminal()
}

/// Iterate visible characters while consuming SGR escape sequences.
fn visible_chars(input: &str) -> impl Iterator<Item = char> + '_ {
    let mut in_escape = false;
    input.chars().filter(move |&ch| {
        if in_escape {
            if ch.is_ascii_alphabetic() {
                in_escape = false;
            }
            false
        } else if ch == '\u{1b}' {
            in_escape = true;
            false
        } else {
            true
        }
    })
}

/// Strip SGR escape sequences from an already-styled line, for the color-off
/// path where the raw codes would otherwise reach the terminal verbatim.
fn strip_ansi(input: &str) -> String {
    visible_chars(input).collect()
}

/// Restores raw mode and cursor visibility on every exit path, including panic.
struct RawGuard;

impl RawGuard {
    fn enter() -> anyhow::Result<Self> {
        terminal::enable_raw_mode()?;
        let mut err = std::io::stderr();
        let _ = queue!(err, cursor::Hide);
        let _ = err.flush();
        Ok(Self)
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        let mut err = std::io::stderr();
        let _ = queue!(err, cursor::Show);
        let _ = err.flush();
        let _ = terminal::disable_raw_mode();
    }
}

impl Picker {
    fn new(question: &str, rows: Vec<PickerRow>, multi: bool) -> Self {
        let checked = rows.iter().map(|row| multi && row.default_on).collect();
        Self {
            question: question.to_string(),
            rows,
            checked,
            cursor: 0,
            multi,
            panel_open: false,
            last_height: 0,
            color: color_enabled(),
        }
    }

    fn id_width(&self) -> usize {
        self.rows
            .iter()
            .map(|row| row.id.chars().count())
            .max()
            .unwrap_or(0)
            .saturating_add(2)
            .max(10)
    }

    /// Assemble the full frame (already-colored complete lines). Every line is
    /// clipped to the terminal width so the redraw's line count never drifts
    /// from the terminal's row count.
    fn frame(&self) -> Vec<String> {
        // A pty can report 0 columns (no size set); treat that as unknown and
        // assume the conventional 80 rather than clipping everything away.
        let cols = match terminal::size() {
            Ok((cols, _)) if cols > 0 => cols as usize,
            _ => 80,
        };
        let side_by_side = cols >= 100 && self.panel_open;
        // Layout floor of 40 keeps the row grammar intact on narrow terminals;
        // the final clip below guarantees no emitted line exceeds `cols`.
        let list_width = if side_by_side {
            (cols * 55 / 100).max(40)
        } else {
            cols.max(40)
        };

        let id_width = self.id_width();
        let mut list: Vec<String> = Vec::new();
        list.push(format!("{} {}", style::accent("◆"), self.question));
        for (idx, row) in self.rows.iter().enumerate() {
            list.push(format!(
                "│  {}",
                render_row(
                    row,
                    id_width,
                    idx == self.cursor,
                    self.checked[idx],
                    self.multi,
                    list_width.saturating_sub(3),
                )
            ));
        }
        let help = if self.multi {
            "space toggle, a all, n none, → details, enter confirm"
        } else {
            "↑↓ move, → details, enter select"
        };
        if !self.panel_open {
            list.push(style::dim("│  → details"));
        }
        list.push(style::dim(format!("│  {help}")));

        let panel = self.panel_lines();

        let lines = if side_by_side {
            merge_columns(&list, &panel, list_width)
        } else if self.panel_open {
            let mut out = list;
            out.push(String::new());
            out.extend(panel);
            out
        } else {
            list
        };
        lines
            .into_iter()
            .map(|line| {
                let clipped = clip_visible(&line, cols);
                if self.color {
                    clipped
                } else {
                    strip_ansi(&clipped)
                }
            })
            .collect()
    }

    fn panel_lines(&self) -> Vec<String> {
        if !self.panel_open {
            return Vec::new();
        }
        let detail = &self.rows[self.cursor].detail;
        let cap = 12usize;
        let mut out: Vec<String> = Vec::new();
        for line in detail.lines.iter().take(cap) {
            out.push(render_panel_line(line));
        }
        if detail.lines.len() > cap {
            out.push(style::dim(format!("… {} more", detail.lines.len() - cap)));
        }
        out
    }

    fn draw(&mut self) -> anyhow::Result<()> {
        let frame = self.frame();
        let mut err = std::io::stderr();
        if self.last_height > 0 {
            queue!(err, cursor::MoveUp(self.last_height))?;
        }
        queue!(err, cursor::MoveToColumn(0))?;
        for line in &frame {
            queue!(err, Clear(ClearType::CurrentLine))?;
            write!(err, "{line}\r\n")?;
        }
        // Clear any leftover lines a taller previous frame left behind.
        let new_height = u16::try_from(frame.len()).unwrap_or(u16::MAX);
        if new_height < self.last_height {
            for _ in new_height..self.last_height {
                queue!(err, Clear(ClearType::CurrentLine))?;
                write!(err, "\r\n")?;
            }
            queue!(err, cursor::MoveUp(self.last_height - new_height))?;
        }
        self.last_height = new_height;
        err.flush()?;
        Ok(())
    }

    fn clear(&mut self) -> anyhow::Result<()> {
        let mut err = std::io::stderr();
        if self.last_height > 0 {
            queue!(err, cursor::MoveUp(self.last_height))?;
        }
        queue!(
            err,
            cursor::MoveToColumn(0),
            Clear(ClearType::FromCursorDown)
        )?;
        err.flush()?;
        self.last_height = 0;
        Ok(())
    }

    fn selected_ids(&self) -> Vec<String> {
        self.rows
            .iter()
            .zip(&self.checked)
            .filter(|(_, checked)| **checked)
            .map(|(row, _)| row.id.clone())
            .collect()
    }

    /// Run the loop. Returns the chosen ids on confirm, or an error on cancel.
    fn run(mut self) -> anyhow::Result<Vec<String>> {
        let _guard = RawGuard::enter()?;
        self.draw()?;
        loop {
            let event = event::read()?;
            // A terminal resize changes the width `frame` clips to; recompute and
            // redraw so the layout tracks the new size instead of corrupting.
            if let Event::Resize(_, _) = event {
                self.draw()?;
                continue;
            }
            let Event::Key(key) = event else {
                continue;
            };
            if key.kind == KeyEventKind::Release {
                continue;
            }
            match key.code {
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return self.cancel();
                },
                KeyCode::Esc => return self.cancel(),
                KeyCode::Up | KeyCode::Char('k') => {
                    if self.cursor == 0 {
                        self.cursor = self.rows.len() - 1;
                    } else {
                        self.cursor -= 1;
                    }
                },
                KeyCode::Down | KeyCode::Char('j') => {
                    self.cursor = (self.cursor + 1) % self.rows.len();
                },
                KeyCode::Char(' ') if self.multi => {
                    self.checked[self.cursor] = !self.checked[self.cursor];
                },
                KeyCode::Char('a') if self.multi => {
                    self.checked.iter_mut().for_each(|c| *c = true);
                },
                KeyCode::Char('n') if self.multi => {
                    self.checked.iter_mut().for_each(|c| *c = false);
                },
                KeyCode::Right => self.panel_open = true,
                KeyCode::Left => self.panel_open = false,
                KeyCode::Tab => self.panel_open = !self.panel_open,
                KeyCode::Enter => {
                    if !self.multi {
                        let id = self.rows[self.cursor].id.clone();
                        self.finish(std::slice::from_ref(&id))?;
                        return Ok(vec![id]);
                    }
                    let ids = self.selected_ids();
                    self.finish(&ids)?;
                    return Ok(ids);
                },
                _ => {},
            }
            self.draw()?;
        }
    }

    fn finish(&mut self, ids: &[String]) -> anyhow::Result<()> {
        self.clear()?;
        drop(std::io::stderr().flush());
        let answer = if ids.is_empty() {
            style::dim("(none)")
        } else {
            style::accent(ids.join(", "))
        };
        anstream::eprintln!("│  {} {} {answer}", style::success("✓"), self.question);
        Ok(())
    }

    fn cancel(&mut self) -> anyhow::Result<Vec<String>> {
        self.clear()?;
        Err(anyhow::Error::new(Canceled))
    }
}

/// Merge a list column and a panel column side by side with a dim separator.
fn merge_columns(list: &[String], panel: &[String], list_width: usize) -> Vec<String> {
    let height = list.len().max(panel.len());
    let sep = style::dim("│");
    (0..height)
        .map(|i| {
            let left = list.get(i).cloned().unwrap_or_default();
            let pad = list_width.saturating_sub(visible_width(&left));
            let right = panel.get(i).cloned().unwrap_or_default();
            format!("{left}{:pad$} {sep} {right}", "", pad = pad)
        })
        .collect()
}

/// Visible width of a string, ignoring SGR escape sequences.
fn visible_width(input: &str) -> usize {
    visible_chars(input).count()
}

/// Clip a possibly-colored line to `max` visible columns, preserving escape
/// sequences and ending with `…` plus an SGR reset when truncated. Keeping
/// every frame line within the terminal width is what stops a line from
/// wrapping and corrupting the cursor-up redraw math.
fn clip_visible(input: &str, max: usize) -> String {
    if visible_width(input) <= max {
        return input.to_string();
    }
    let budget = max.saturating_sub(1);
    let mut out = String::with_capacity(input.len());
    let mut visible = 0;
    let mut chars = input.chars();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            out.push(ch);
            for next in chars.by_ref() {
                out.push(next);
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            if visible == budget {
                break;
            }
            out.push(ch);
            visible += 1;
        }
    }
    out.push('…');
    // Close any styling that was cut off mid-span.
    out.push_str("\u{1b}[0m");
    out
}

/// Multi-select entry point.
pub(crate) fn multiselect(question: &str, rows: Vec<PickerRow>) -> anyhow::Result<Vec<String>> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    Picker::new(question, rows, true).run()
}

/// Single-select entry point.
pub(crate) fn select(question: &str, rows: Vec<PickerRow>) -> anyhow::Result<String> {
    if rows.is_empty() {
        anyhow::bail!("no providers available to choose from");
    }
    let ids = Picker::new(question, rows, false).run()?;
    ids.into_iter()
        .next()
        .ok_or_else(|| anyhow!("no provider selected"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canceled_is_detected_only_for_the_marker() {
        assert!(is_canceled(&anyhow::Error::new(Canceled)));
        assert!(!is_canceled(&anyhow!("some other failure")));
    }

    #[test]
    fn clip_visible_caps_visible_width_and_preserves_escapes() {
        // Plain text.
        assert_eq!(clip_visible("hello", 10), "hello");
        let clipped = clip_visible("hello world", 8);
        assert_eq!(visible_width(&clipped), 8);
        assert!(strip_ansi(&clipped).starts_with("hello w"));

        // Colored text: escapes do not count against the budget and the clip
        // ends with a reset.
        let colored = format!("{} tail that overflows", style::dim("dimmed prefix"));
        let clipped = clip_visible(&colored, 10);
        assert_eq!(visible_width(&clipped), 10);
        assert!(clipped.ends_with("\u{1b}[0m"));

        // Multibyte characters count as characters, not bytes.
        let clipped = clip_visible("éclair", 4);
        assert_eq!(visible_width(&clipped), 4);
        assert_eq!(strip_ansi(&clipped), "écl…");
    }

    // Archetype manifests for the four `default_on` branches. The names mirror
    // the shipped providers, though the shipped `linear` actually defaults to
    // OAuth; this test pins the predicate, using a static-token default to
    // represent the "needs an API key, no ambient" case.
    mod archetypes {
        use omnifs_caps::PreopenMode;
        use omnifs_workspace::authn::{
            AmbientSource, AuthScheme, OAuthFlow, OauthScheme, PkceLoopbackConfig,
            StaticTokenScheme, TokenEndpointAuthMethod,
        };
        use omnifs_workspace::provider::{
            ConfigField, ConfigMetadata, ConfigType, HostResourceBinding, ProviderAuthManifest,
            ProviderManifest,
        };
        use std::collections::BTreeMap;

        fn base(id: &str) -> ProviderManifest {
            ProviderManifest {
                id: id.to_string(),
                display_name: id.to_string(),
                description: None,
                provider: format!("omnifs_provider_{id}.wasm"),
                default_mount: id.to_string(),
                version: None,
                wit_package: None,
                sdk_version: None,
                capabilities: vec![],
                limits: omnifs_caps::LimitDeclarations::default(),
                auth: None,
                config: None,
            }
        }

        fn oauth_scheme() -> AuthScheme {
            AuthScheme::Oauth(OauthScheme {
                key: "oauth".to_string(),
                display_name: "OAuth".to_string(),
                authorization_endpoint: "https://example.com/authorize".to_string(),
                token_endpoint: "https://example.com/token".to_string(),
                revocation_endpoint: None,
                default_client_id: Some("cid".to_string()),
                default_scopes: vec![],
                flow: OAuthFlow::PkceLoopback(PkceLoopbackConfig {
                    redirect_uri_template: "http://127.0.0.1:{port}/cb".to_string(),
                }),
                token_endpoint_auth: TokenEndpointAuthMethod::None,
                refresh_token_rotates: false,
                extra_authorize_params: vec![],
                extra_token_params: vec![],
                inject_domains: vec!["api.example.com".to_string()],
                inject_header_name: Some("Authorization".to_string()),
                inject_value_prefix: String::new(),
            })
        }

        fn static_scheme(ambient: Vec<AmbientSource>) -> AuthScheme {
            AuthScheme::StaticToken(StaticTokenScheme {
                key: "pat".to_string(),
                header_name: Some("Authorization".to_string()),
                value_prefix: String::new(),
                description: "API key".to_string(),
                inject_domains: vec!["api.example.com".to_string()],
                creation_url: None,
                validation: None,
                ambient_sources: ambient,
            })
        }

        pub(super) fn github() -> ProviderManifest {
            let mut manifest = base("github");
            manifest.auth = Some(ProviderAuthManifest {
                default: "oauth".to_string(),
                schemes: vec![oauth_scheme()],
                guidance: BTreeMap::new(),
            });
            manifest
        }

        pub(super) fn arxiv() -> ProviderManifest {
            base("arxiv")
        }

        pub(super) fn db() -> ProviderManifest {
            let mut manifest = base("db");
            manifest.config = Some(ConfigMetadata {
                fields: vec![ConfigField {
                    name: "path".to_string(),
                    value_type: ConfigType::String,
                    required: true,
                    default: None,
                    description: None,
                    binding: Some(HostResourceBinding::File {
                        mode: PreopenMode::Ro,
                    }),
                }],
            });
            manifest
        }

        pub(super) fn linear() -> ProviderManifest {
            let mut manifest = base("linear");
            manifest.auth = Some(ProviderAuthManifest {
                default: "pat".to_string(),
                schemes: vec![static_scheme(vec![])],
                guidance: BTreeMap::new(),
            });
            manifest
        }
    }

    #[test]
    #[allow(unsafe_code)]
    fn default_on_pins_the_four_branches() {
        // Guard against a stray ambient credential in the test env.
        let _guard = env_lock();
        // SAFETY: serialized by env_lock; no other test mutates these vars.
        unsafe {
            std::env::remove_var("LINEAR_API_KEY");
        }
        assert!(default_on(&archetypes::github()), "oauth default -> on");
        assert!(default_on(&archetypes::arxiv()), "no auth -> on");
        assert!(!default_on(&archetypes::db()), "config prompt -> off");
        assert!(
            !default_on(&archetypes::linear()),
            "static default, no ambient -> off"
        );
    }

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn render_row_never_wraps() {
        let row = PickerRow {
            id: "github".to_string(),
            summary: "repos, issues, and pull requests are projected as files".to_string(),
            cap_tags: vec![CapTag::Net, CapTag::Git],
            auth_tag: Some(AuthTag::SignIn),
            default_on: true,
            detail: Detail { lines: vec![] },
        };
        let rendered = render_row(&row, 10, true, true, true, 80);
        assert!(strip_ansi(&rendered).chars().count() <= 80);
    }
}
