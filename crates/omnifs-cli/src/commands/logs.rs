#![allow(clippy::disallowed_macros)] // permanent: raw log passthrough
//! `omnifs logs` — tail the daemon's log file. Content on stdout
//! is the daemon log verbatim, never restructured; the one narration line
//! (following vs. down, and the empty state) is stderr.

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::Context as _;
use clap::Args;

use crate::ui::output::Output;
use crate::ui::style::{self, Stream};
use omnifs_workspace::Workspace;

#[derive(Args, Debug, Clone, Default)]
pub struct LogsArgs {
    #[arg(short = 'f', long)]
    pub follow: bool,
}

/// Default tail length: last 50 lines when not following.
const DEFAULT_TAIL_LINES: usize = 50;

impl LogsArgs {
    pub fn run(self, output: &Output) -> anyhow::Result<()> {
        if output.is_structured() {
            anyhow::bail!("logs is a passthrough command and only supports human output")
        }
        let workspace = Workspace::resolve()?;
        let log_path = workspace.daemon().log_file();
        if !log_path.exists() {
            output.narrate("No daemon log yet. It's written on first `omnifs up`.");
            return Ok(());
        }
        let running = daemon_is_running(&workspace);
        output.narrate(style::dim(
            header_line(&log_path, self.follow, running),
            Stream::Stderr,
        ));
        let color = style::color_enabled(Stream::Stdout);
        if self.follow && running {
            follow_native_log(&log_path, color)
        } else {
            print_static_tail(&log_path, color)
        }
    }
}

/// Whether the daemon that owns this log is currently alive, best-effort: a
/// live process for the recorded pid. A false positive/negative here only
/// affects the narration header, never the log content itself.
fn daemon_is_running(workspace: &Workspace) -> bool {
    workspace
        .daemon()
        .record()
        .ok()
        .flatten()
        .is_some_and(|record| crate::process::is_alive(record.pid))
}

/// The one stderr header line. Pure so the exact wording is
/// testable without a live daemon.
fn header_line(log_path: &Path, follow: bool, running: bool) -> String {
    if !running {
        "daemon is not running; showing its last log".to_owned()
    } else if follow {
        format!(
            "tailing {}  (^C to stop)",
            omnifs_workspace::display(log_path)
        )
    } else {
        format!(
            "showing the last {DEFAULT_TAIL_LINES} lines of {}",
            omnifs_workspace::display(log_path)
        )
    }
}

/// Print the last `DEFAULT_TAIL_LINES` lines once and return; used both for
/// the plain (non-`-f`) case and as `-f`'s degrade when the daemon is down
/// (nothing new is coming, so following would just hang).
fn print_static_tail(log_path: &Path, color: bool) -> anyhow::Result<()> {
    let contents = std::fs::read(log_path)
        .with_context(|| format!("read daemon log {}", log_path.display()))?;
    let tail = &contents[tail_start(&contents, DEFAULT_TAIL_LINES)..];
    if !color {
        std::io::stdout()
            .lock()
            .write_all(tail)
            .context("write daemon log")?;
        return Ok(());
    }
    let text = String::from_utf8_lossy(tail);
    let mut stdout = std::io::stdout().lock();
    for line in text.split_inclusive('\n') {
        let (body, ending) = line.strip_suffix('\n').map_or((line, ""), |body| {
            body.strip_suffix('\r')
                .map_or((body, "\n"), |body| (body, "\r\n"))
        });
        stdout.write_all(colorize_log_line(body, true).as_bytes())?;
        stdout.write_all(ending.as_bytes())?;
    }
    Ok(())
}

fn tail_start(contents: &[u8], line_count: usize) -> usize {
    if contents.is_empty() || line_count == 0 {
        return contents.len();
    }
    let mut breaks = 0;
    for index in (0..contents.len()).rev() {
        if contents[index] != b'\n' || index + 1 == contents.len() {
            continue;
        }
        breaks += 1;
        if breaks == line_count {
            return index + 1;
        }
    }
    0
}

/// Stream new lines as they're written, surviving log rotation via `tail
/// -F`. Reads the child's stdout ourselves (rather than letting `tail`
/// inherit it directly) so each line gets the same TTY-only level coloring
/// as the static tail.
fn follow_native_log(log_path: &Path, color: bool) -> anyhow::Result<()> {
    if !color {
        let status = Command::new("tail")
            .arg("-F")
            .arg("-n")
            .arg(DEFAULT_TAIL_LINES.to_string())
            .arg(log_path)
            .status()
            .with_context(|| format!("tail -F {}", log_path.display()))?;
        anyhow::ensure!(status.success(), "tail -F exited with {status}");
        return Ok(());
    }
    let mut child = Command::new("tail")
        .arg("-F")
        .arg("-n")
        .arg(DEFAULT_TAIL_LINES.to_string())
        .arg(log_path)
        .stdout(Stdio::piped())
        .spawn()
        .with_context(|| format!("tail -F {}", log_path.display()))?;
    let stdout = child.stdout.take().expect("stdout was piped");
    let mut reader = BufReader::new(stdout);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        // Read raw bytes and decode lossily: a single non-UTF-8 byte in the
        // log must not end the follow (`BufRead::lines` would error on it),
        // and a real I/O error must not leave us blocked in `wait()` on a
        // `tail -F` that never exits on its own.
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) => break,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error).context("read tail output");
            },
            Ok(_) => {
                let line = String::from_utf8_lossy(&buf);
                let line = line.trim_end_matches(['\n', '\r']);
                crate::ui::print_raw(&format!("{}\n", colorize_log_line(line, color)));
                let _ = std::io::stdout().flush();
            },
        }
    }
    let status = child.wait().context("wait for tail")?;
    anyhow::ensure!(status.success(), "tail -F exited with {status}");
    Ok(())
}

/// TTY-only level coloring: `ERROR` red, `WARN` yellow, a leading
/// RFC3339-shaped timestamp token dim. Splits and rejoins on a single space
/// so exact spacing round-trips; `color = false` (piped output) returns the
/// line untouched.
fn colorize_log_line(line: &str, color: bool) -> String {
    if !color || line.is_empty() {
        return line.to_owned();
    }
    let mut words = line.split(' ');
    let mut out = Vec::new();
    if let Some(first) = words.next() {
        out.push(if looks_like_timestamp(first) {
            style::dim(first, color)
        } else {
            level_colored(first, color)
        });
    }
    for word in words {
        out.push(level_colored(word, color));
    }
    out.join(" ")
}

fn level_colored(word: &str, color: bool) -> String {
    match word {
        "ERROR" => style::error(word, color),
        "WARN" => style::warn(word, color),
        _ => word.to_owned(),
    }
}

/// A loose RFC3339-shaped heuristic: tracing's default formatter opens every
/// line with a UTC timestamp like `2026-07-18T10:00:00.123456Z`.
fn looks_like_timestamp(token: &str) -> bool {
    token.len() >= 10
        && token.contains('T')
        && token
            .chars()
            .all(|ch| ch.is_ascii_digit() || matches!(ch, ':' | '-' | '.' | 'T' | 'Z'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_line_names_following_down_and_the_default_case() {
        let path = Path::new("/home/u/.omnifs/cache/daemon.log");

        assert_eq!(
            header_line(path, false, false),
            "daemon is not running; showing its last log"
        );
        // A down daemon wins over `-f`: nothing new is coming, so the
        // "tailing" framing would be misleading.
        assert_eq!(
            header_line(path, true, false),
            "daemon is not running; showing its last log"
        );
        assert!(header_line(path, true, true).starts_with("tailing "));
        assert!(header_line(path, true, true).ends_with("  (^C to stop)"));
        assert_eq!(
            header_line(path, false, true),
            format!(
                "showing the last {DEFAULT_TAIL_LINES} lines of {}",
                omnifs_workspace::display(path)
            )
        );
    }

    #[test]
    fn colorize_log_line_is_a_passthrough_when_color_is_off() {
        let line = "2026-07-18T10:00:00.123456Z ERROR provider github: boom";
        assert_eq!(colorize_log_line(line, false), line);
        assert_eq!(colorize_log_line("", true), "");
    }

    #[test]
    fn colorize_log_line_dims_the_timestamp_and_colors_the_level() {
        let line = "2026-07-18T10:00:00.123456Z ERROR provider github: boom";
        let rendered = colorize_log_line(line, true);
        assert_eq!(
            rendered,
            format!(
                "{} {} provider github: boom",
                style::dim("2026-07-18T10:00:00.123456Z", true),
                style::error("ERROR", true)
            )
        );

        let warn_line = "2026-07-18T10:00:01.000000Z WARN cache stale";
        let rendered = colorize_log_line(warn_line, true);
        assert!(rendered.contains(&style::warn("WARN", true)));
    }

    #[test]
    fn colorize_log_line_preserves_exact_spacing() {
        let line = "INFO   double  spaced";
        assert_eq!(colorize_log_line(line, false), line);
        // Splitting and rejoining on a single space must round-trip runs of
        // spaces exactly, not collapse them.
        assert_eq!(colorize_log_line(line, true), line);
    }

    #[test]
    fn looks_like_timestamp_accepts_rfc3339_and_rejects_ordinary_words() {
        assert!(looks_like_timestamp("2026-07-18T10:00:00.123456Z"));
        assert!(!looks_like_timestamp("ERROR"));
        assert!(!looks_like_timestamp("short"));
    }

    #[test]
    fn tail_start_preserves_raw_line_endings_and_final_newline_state() {
        let bytes = b"a\r\nb\xff\nc";
        assert_eq!(&bytes[tail_start(bytes, 2)..], b"b\xff\nc");
        let terminated = b"a\r\nb\r\nc\r\n";
        assert_eq!(&terminated[tail_start(terminated, 2)..], b"b\r\nc\r\n");
        assert_eq!(tail_start(bytes, 0), bytes.len());
    }
}
