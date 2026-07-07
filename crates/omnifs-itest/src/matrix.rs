//! Frontend conformance matrix: the shared row table, outcome model, per-column
//! scorecard, executor abstraction, and markdown renderer.
//!
//! The product contract is that the projected tree behaves like real files for
//! the standard toolbox. This module encodes that contract as a table of shell
//! rows and runs it against a live mount through an [`Exec`] lane (local process
//! or `docker exec`). Each lane produces a [`Scorecard`] whose per-row observed
//! outcome is checked against a per-column [`Expect`]; a run is red iff any row
//! contradicts its expectation. The row table and models live here (not in a
//! test target) so future runtime-redesign phases reuse them.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Serialize;

/// The runner-side kill deadline for a single row. A wedged frontend operation
/// (a mount that never answers) must not hang the whole lane; on timeout the row
/// is recorded as failed with error `timeout`.
const ROW_TIMEOUT: Duration = Duration::from_mins(1);

// ===========================================================================
// Row model
// ===========================================================================

/// Whether a row exercises read semantics or a write/mutation. Write rows expect
/// failure today (the frontends are read-only) and flip green the day the write
/// path lands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowClass {
    Read,
    Write,
}

impl RowClass {
    fn as_str(self) -> &'static str {
        match self {
            RowClass::Read => "read",
            RowClass::Write => "write",
        }
    }
}

/// One conformance row: a Bourne-shell script run with `cwd=$SCRATCH` and the
/// `ROOT` and `SCRATCH` env vars set. Exit 0 means the operation behaved like a
/// real filesystem.
///
/// DANGER: never scope a recursive row (`grep -r`, `find`, `du`, `rsync`) at
/// `$ROOT` or `$ROOT/hello`. The test provider serves `hello/unbounded` (pages
/// forever), `hello/throttled` (always rate-limits), and `slow/<ms>` (sleeps).
/// Recursive rows must target `$ROOT/items` or `$ROOT/hello/bundle` only, both
/// of which are exhaustive, bounded listings.
pub struct Row {
    pub id: &'static str,
    pub class: RowClass,
    /// Optional tool the row needs beyond the POSIX base. When set, the executor
    /// probes `command -v <tool>` first and records [`Outcome::ToolMissing`]
    /// (an environment gap, not a frontend result) when it is absent.
    pub tool: Option<&'static str>,
    pub script: &'static str,
}

// ===========================================================================
// Outcome + expectation model
// ===========================================================================

/// The observed result of running a row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Pass,
    Fail { error: String },
    ToolMissing { tool: String },
}

impl Outcome {
    fn as_str(&self) -> &'static str {
        match self {
            Outcome::Pass => "pass",
            Outcome::Fail { .. } => "fail",
            Outcome::ToolMissing { .. } => "tool-missing",
        }
    }

    fn error(&self) -> Option<&str> {
        match self {
            Outcome::Fail { error } => Some(error),
            Outcome::ToolMissing { .. } | Outcome::Pass => None,
        }
    }
}

/// What a column expects a row to do. A [`Expect::Fail`] entry names a known
/// read-only or frontend-quirk limitation; each carries a one-line rationale
/// where it is declared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expect {
    Pass,
    Fail,
}

impl Expect {
    fn as_str(self) -> &'static str {
        match self {
            Expect::Pass => "pass",
            Expect::Fail => "fail",
        }
    }
}

/// An execution lane: one column of the matrix. Expectations override the class
/// baseline (read -> pass, write -> fail) per row id, for the frontend quirks a
/// live run proves.
pub struct Column {
    pub id: &'static str,
    pub platform: &'static str,
    /// Per-row expectation overrides. A row id absent here uses the class
    /// baseline. Each entry that flips a read row to [`Expect::Fail`] carries a
    /// rationale comment at the declaration site.
    pub expectations: &'static [(&'static str, Expect)],
}

impl Column {
    /// The expectation for `row`: an override if present, else the class
    /// baseline (read -> pass, write -> fail; the frontends are read-only).
    #[must_use]
    pub fn expectation(&self, row: &Row) -> Expect {
        for (id, expect) in self.expectations {
            if *id == row.id {
                return *expect;
            }
        }
        match row.class {
            RowClass::Read => Expect::Pass,
            RowClass::Write => Expect::Fail,
        }
    }
}

/// Whether an observed outcome contradicts an expectation. A tool gap satisfies
/// any expectation (it is an environment gap, not frontend behavior). Otherwise
/// a pass must match [`Expect::Pass`] and a fail must match [`Expect::Fail`] —
/// an unexpectedly passing expected-fail row is a mismatch too, so the table
/// stays honest.
#[must_use]
pub fn is_mismatch(outcome: &Outcome, expect: Expect) -> bool {
    match outcome {
        Outcome::ToolMissing { .. } => false,
        Outcome::Pass => expect != Expect::Pass,
        Outcome::Fail { .. } => expect != Expect::Fail,
    }
}

// ===========================================================================
// Executor
// ===========================================================================

/// Where and how a row's script runs. Both variants run the script through
/// `sh -c` with `ROOT`/`SCRATCH` in the environment and `cwd=$SCRATCH`.
pub enum Exec {
    /// A local shell against a host-visible mount.
    Local { root: PathBuf, scratch: PathBuf },
    /// `docker exec` into a running container serving the mount. `root` and
    /// `scratch` are guest paths.
    DockerExec {
        container: String,
        root: String,
        scratch: String,
    },
}

impl Exec {
    fn root(&self) -> &str {
        match self {
            Exec::Local { root, .. } => root.to_str().expect("root path is utf-8"),
            Exec::DockerExec { root, .. } => root,
        }
    }

    fn scratch(&self) -> &str {
        match self {
            Exec::Local { scratch, .. } => scratch.to_str().expect("scratch path is utf-8"),
            Exec::DockerExec { scratch, .. } => scratch,
        }
    }

    /// Build the `sh -c <script>` command for this lane, with `ROOT`, `SCRATCH`,
    /// and `cwd=$SCRATCH`.
    fn command(&self, script: &str) -> Command {
        let root = self.root().to_string();
        let scratch = self.scratch().to_string();
        match self {
            Exec::Local { .. } => {
                let mut cmd = Command::new("sh");
                cmd.args(["-c", script])
                    .current_dir(&scratch)
                    .env("ROOT", &root)
                    .env("SCRATCH", &scratch);
                cmd
            },
            Exec::DockerExec { container, .. } => {
                let mut cmd = Command::new("docker");
                cmd.args([
                    "exec",
                    "-w",
                    &scratch,
                    "-e",
                    &format!("ROOT={root}"),
                    "-e",
                    &format!("SCRATCH={scratch}"),
                    container,
                    "sh",
                    "-c",
                    script,
                ]);
                cmd
            },
        }
    }

    /// Run one row: probe its optional tool, run the script under a runner-side
    /// timeout, and classify the outcome.
    #[must_use]
    pub fn run(&self, row: &Row) -> Outcome {
        if let Some(tool) = row.tool
            && !self.tool_present(tool)
        {
            return Outcome::ToolMissing {
                tool: tool.to_string(),
            };
        }
        let result = run_with_timeout(self.command(row.script), ROW_TIMEOUT);
        match result {
            RunResult::TimedOut => Outcome::Fail {
                error: "timeout".to_string(),
            },
            RunResult::Exited { success: true, .. } => Outcome::Pass,
            RunResult::Exited {
                success: false,
                code,
                stderr,
            } => Outcome::Fail {
                error: first_error_line(&stderr, code),
            },
        }
    }

    /// Probe `command -v <tool>` through the same lane.
    fn tool_present(&self, tool: &str) -> bool {
        let script = format!("command -v {tool} >/dev/null 2>&1");
        matches!(
            run_with_timeout(self.command(&script), ROW_TIMEOUT),
            RunResult::Exited { success: true, .. }
        )
    }
}

/// The lane-neutral timing wrapper: the total wall-clock of one [`Exec::run`],
/// paired with its outcome. Used to fill the scorecard's `duration_ms`.
#[must_use]
pub fn run_timed(exec: &Exec, row: &Row) -> (Outcome, u64) {
    let started = Instant::now();
    let outcome = exec.run(row);
    let elapsed = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    (outcome, elapsed)
}

enum RunResult {
    TimedOut,
    Exited {
        success: bool,
        code: Option<i32>,
        stderr: Vec<u8>,
    },
}

/// Spawn `cmd` with piped stdout/stderr, drain both on reader threads so a
/// chatty child cannot deadlock on a full pipe, and kill it if it outlives
/// `timeout`. stdout is drained and dropped; only stderr feeds the error line.
fn run_with_timeout(mut cmd: Command, timeout: Duration) -> RunResult {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) => {
            return RunResult::Exited {
                success: false,
                code: None,
                stderr: format!("spawn failed: {error}").into_bytes(),
            };
        },
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_reader = drain(stdout);
    let err_reader = drain(stderr);

    let deadline = Instant::now() + timeout;
    let timed_out = loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => break false,
            Ok(None) => {},
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            break true;
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    let status = child.wait();
    let _ = out_reader.join();
    let stderr_bytes = err_reader.join().unwrap_or_default();

    if timed_out {
        return RunResult::TimedOut;
    }
    match status {
        Ok(status) => RunResult::Exited {
            success: status.success(),
            code: status.code(),
            stderr: stderr_bytes,
        },
        Err(error) => RunResult::Exited {
            success: false,
            code: None,
            stderr: format!("wait failed: {error}").into_bytes(),
        },
    }
}

fn drain(pipe: Option<impl Read + Send + 'static>) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut buf);
        }
        buf
    })
}

/// The first non-empty line of stderr, or a terse `exit status N` when the
/// child failed silently.
fn first_error_line(stderr: &[u8], code: Option<i32>) -> String {
    let text = String::from_utf8_lossy(stderr);
    for line in text.lines() {
        let line = line.trim();
        if !line.is_empty() {
            return line.to_string();
        }
    }
    match code {
        Some(code) => format!("exit status {code}"),
        None => "terminated by signal".to_string(),
    }
}

// ===========================================================================
// Scorecard
// ===========================================================================

#[derive(Serialize)]
pub struct Scorecard {
    pub version: u32,
    pub column: String,
    pub platform: String,
    pub generated_at: String,
    pub rows: Vec<RowResult>,
}

#[derive(Serialize)]
pub struct RowResult {
    pub id: String,
    pub class: String,
    pub outcome: String,
    pub expected: String,
    pub duration_ms: u64,
    pub error: Option<String>,
}

impl RowResult {
    fn observed(&self) -> Observed<'_> {
        Observed {
            outcome: self.outcome.as_str(),
            expected: self.expected.as_str(),
        }
    }
}

/// The observed cell state used by the renderer, decoupled from the parsed
/// [`Outcome`]/[`Expect`] so the renderer works from a serialized scorecard too.
struct Observed<'a> {
    outcome: &'a str,
    expected: &'a str,
}

/// Run every row in `rows` on `exec` for `column`, returning its scorecard. The
/// caller writes the scorecard and prints the table before asserting, so a red
/// run still leaves evidence.
#[must_use]
pub fn run_column(exec: &Exec, column: &Column, rows: &[Row]) -> Scorecard {
    let results = rows
        .iter()
        .map(|row| {
            let (outcome, duration_ms) = run_timed(exec, row);
            let expected = column.expectation(row);
            RowResult {
                id: row.id.to_string(),
                class: row.class.as_str().to_string(),
                outcome: outcome.as_str().to_string(),
                expected: expected.as_str().to_string(),
                duration_ms,
                error: outcome.error().map(str::to_string),
            }
        })
        .collect();

    Scorecard {
        version: 1,
        column: column.id.to_string(),
        platform: column.platform.to_string(),
        generated_at: now_rfc3339(),
        rows: results,
    }
}

fn now_rfc3339() -> String {
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;

    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "unknown".to_string())
}

/// The scorecard directory: `$OMNIFS_SCORECARD_DIR`, else
/// `<workspace>/target/conformance/`. Created if absent.
#[must_use]
pub fn scorecard_dir() -> PathBuf {
    let dir = std::env::var_os("OMNIFS_SCORECARD_DIR").map_or_else(
        || crate::workspace_root().join("target/conformance"),
        PathBuf::from,
    );
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Write `scorecard` to `<scorecard_dir>/scorecard-<column>.json` and return the
/// path it landed at.
pub fn write_scorecard(scorecard: &Scorecard) -> PathBuf {
    let dir = scorecard_dir();
    let path = dir.join(format!("scorecard-{}.json", scorecard.column));
    let json = serde_json::to_string_pretty(scorecard).expect("serialize scorecard");
    std::fs::write(&path, json).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    path
}

/// Row ids that contradicted their expectation, as human-readable strings. Empty
/// iff the column is fully honest.
#[must_use]
pub fn mismatches(scorecard: &Scorecard) -> Vec<String> {
    scorecard
        .rows
        .iter()
        .filter(|row| cell_is_mismatch(&row.observed()))
        .map(|row| {
            let error = row
                .error
                .as_deref()
                .map(|e| format!(" ({e})"))
                .unwrap_or_default();
            format!(
                "{}: observed {}, expected {}{error}",
                row.id, row.outcome, row.expected
            )
        })
        .collect()
}

// ===========================================================================
// Markdown renderer
// ===========================================================================

/// A GitHub-flavored markdown table, rows x columns. Cell values are
/// `pass`/`FAIL`/`xfail`/`tool-missing`; mismatches (observed contradicts
/// expected) are bolded.
#[must_use]
pub fn render_table(scorecards: &[Scorecard]) -> String {
    use std::fmt::Write as _;

    if scorecards.is_empty() {
        return String::new();
    }

    // Row order follows the first scorecard; every lane runs the same table.
    let row_ids: Vec<&str> = scorecards[0].rows.iter().map(|r| r.id.as_str()).collect();

    let mut out = String::new();
    out.push_str("| row |");
    for card in scorecards {
        let _ = write!(out, " {} |", card.column);
    }
    out.push_str("\n| --- |");
    for _ in scorecards {
        out.push_str(" --- |");
    }
    out.push('\n');

    for id in &row_ids {
        let _ = write!(out, "| {id} |");
        for card in scorecards {
            let cell = card
                .rows
                .iter()
                .find(|r| &r.id == id)
                .map_or_else(|| "—".to_string(), |r| render_cell(&r.observed()));
            let _ = write!(out, " {cell} |");
        }
        out.push('\n');
    }
    out
}

fn cell_is_mismatch(observed: &Observed) -> bool {
    match observed.outcome {
        "tool-missing" => false,
        "pass" => observed.expected != "pass",
        _ => observed.expected != "fail",
    }
}

fn render_cell(observed: &Observed) -> String {
    let text = match (observed.outcome, observed.expected) {
        ("tool-missing", _) => "tool-missing",
        ("fail", "fail") => "xfail",
        ("fail", _) => "FAIL",
        // pass (expected or not): a green cell, bolded below when unexpected.
        _ => "pass",
    };
    if cell_is_mismatch(observed) {
        format!("**{text}**")
    } else {
        text.to_string()
    }
}

// ===========================================================================
// The row table
// ===========================================================================

/// The frontend conformance rows: the standard toolbox against the test
/// provider's canned tree. Read rows prove real-filesystem behavior; write rows
/// assert the write SUCCEEDS, so each flips green the day the write path lands.
///
/// DANGER: recursive rows (`grep -r`, `find`, `du`, `rsync`) are scoped to
/// `$ROOT/items` or `$ROOT/hello/bundle` only — `$ROOT/hello/unbounded`,
/// `hello/throttled`, and `slow/<ms>` never terminate.
pub const ROWS: &[Row] = &[
    // --- reads -------------------------------------------------------------
    Row {
        id: "cat",
        class: RowClass::Read,
        tool: None,
        script: r#"[ "$(cat "$ROOT/hello/message")" = "Hello, world!" ]"#,
    },
    Row {
        id: "head-c",
        class: RowClass::Read,
        tool: None,
        script: r#"[ "$(head -c 4 "$ROOT/hello/ranged")" = "abcd" ]"#,
    },
    Row {
        id: "tail-c",
        class: RowClass::Read,
        tool: None,
        script: r#"[ "$(tail -c 4 "$ROOT/hello/ranged")" = "wxyz" ]"#,
    },
    Row {
        id: "tail-n",
        class: RowClass::Read,
        tool: None,
        script: r#"[ "$(tail -n 1 "$ROOT/items/open/7/item.md")" = "Body 7" ]"#,
    },
    Row {
        // hello/live-log grows one 12-byte line per 500ms from its first read.
        // The initial content at open (~t0) is 1-2 lines, so capturing 3 or
        // more proves tail observed APPENDED bytes, not just the opening
        // snapshot. volatile-tail is deliberately not used here: it fabricates
        // bytes at any offset and never signals the current end, so a
        // follower's initial scan-to-EOF never terminates.
        id: "tail-f-growing",
        class: RowClass::Read,
        tool: None,
        script: r#"tail -f "$ROOT/hello/live-log" > "$SCRATCH/tailf.out" 2> "$SCRATCH/tailf.err" &
pid=$!
sleep 5
kill "$pid" 2>/dev/null
wait "$pid" 2>/dev/null
lines=$(wc -l < "$SCRATCH/tailf.out" | tr -d ' ')
[ "$lines" -ge 3 ] || {
  # Mechanism probe for the failure report: GNU tail in explicit stat-polling
  # mode distinguishes "growth is invisible" from "growth is inotify-invisible".
  # stdbuf -o0 defeats stdio buffering so the killed tail's output is not lost.
  timeout 5 stdbuf -o0 tail ---disable-inotify -f "$ROOT/hello/live-log" > "$SCRATCH/tailf.poll" 2>/dev/null
  polling=$(wc -l < "$SCRATCH/tailf.poll" 2>/dev/null | tr -d ' ')
  echo "captured $lines line(s), $(wc -c < "$SCRATCH/tailf.out" | tr -d ' ') byte(s): [$(head -c 72 "$SCRATCH/tailf.out" | tr '\n' '|')]; polling-mode lines: ${polling:-n/a}; tail stderr: $(cat "$SCRATCH/tailf.err")" >&2
  exit 1
}"#,
    },
    Row {
        id: "grep-r",
        class: RowClass::Read,
        tool: None,
        script: r#"grep -r "Body 7" "$ROOT/items/open/7" >/dev/null"#,
    },
    Row {
        id: "find-name",
        class: RowClass::Read,
        tool: None,
        script: r#"[ "$(find "$ROOT/items" -name item.json | wc -l | tr -d ' ')" = "4" ]"#,
    },
    Row {
        id: "find-type",
        class: RowClass::Read,
        tool: None,
        script: r#"[ "$(find "$ROOT/items/open" -type d | wc -l | tr -d ' ')" -ge 3 ]"#,
    },
    Row {
        // Scoped to hello/bundle (exhaustive, bounded) rather than hello/,
        // whose open listing does not enumerate leaves. Byte units (`c`) are
        // exact on both BSD and GNU find, whereas block-ish units (`-1k`)
        // round sizes up to whole units on GNU find and match only empty
        // files. The generous bound also tolerates the not-yet-learned size
        // sentinel a cold stat may report.
        id: "find-size",
        class: RowClass::Read,
        tool: None,
        script: r#"[ -n "$(find "$ROOT/hello/bundle" -maxdepth 1 -name title -size -1000c)" ]"#,
    },
    Row {
        id: "ls-l",
        class: RowClass::Read,
        tool: None,
        script: r#"[ "$(ls -l "$ROOT/hello/message" | awk '{print $5}')" = "13" ]"#,
    },
    Row {
        id: "du-sh",
        class: RowClass::Read,
        tool: None,
        script: r#"out=$(du -sh "$ROOT/hello/bundle") && [ -n "$out" ]"#,
    },
    Row {
        id: "stat-size",
        class: RowClass::Read,
        tool: None,
        script: r#"sz=$(stat -f%z "$ROOT/hello/message" 2>/dev/null || stat -c%s "$ROOT/hello/message"); [ "$sz" = "13" ]"#,
    },
    Row {
        id: "wc-c",
        class: RowClass::Read,
        tool: None,
        script: r#"[ "$(wc -c < "$ROOT/hello/message" | tr -d ' ')" = "13" ]"#,
    },
    Row {
        id: "cp",
        class: RowClass::Read,
        tool: None,
        script: r#"cp "$ROOT/hello/message" "$SCRATCH/msg" && printf 'Hello, world!' > "$SCRATCH/ref" && cmp "$SCRATCH/msg" "$SCRATCH/ref""#,
    },
    Row {
        id: "tar",
        class: RowClass::Read,
        tool: None,
        script: r#"tar -C "$ROOT/hello/bundle" -cf "$SCRATCH/b.tar" title body || { echo "create: $?" >&2; exit 1; }
tar -tf "$SCRATCH/b.tar" > "$SCRATCH/list" || { echo "list: $?" >&2; exit 1; }
grep -q title "$SCRATCH/list" || { echo "list lacks title: $(tr '\n' ' ' < "$SCRATCH/list")" >&2; exit 1; }
grep -q body "$SCRATCH/list" || { echo "list lacks body: $(tr '\n' ' ' < "$SCRATCH/list")" >&2; exit 1; }
mkdir -p "$SCRATCH/x"
tar -C "$SCRATCH/x" -xf "$SCRATCH/b.tar" || { echo "extract: $?" >&2; exit 1; }
[ "$(cat "$SCRATCH/x/title")" = "title" ] || { echo "title content: $(cat "$SCRATCH/x/title")" >&2; exit 1; }
[ "$(cat "$SCRATCH/x/body")" = "body" ] || { echo "body content: $(cat "$SCRATCH/x/body")" >&2; exit 1; }"#,
    },
    Row {
        id: "rsync",
        class: RowClass::Read,
        tool: Some("rsync"),
        script: r#"rsync -r "$ROOT/hello/bundle/" "$SCRATCH/bundle-copy/" \
  && [ "$(cat "$SCRATCH/bundle-copy/title")" = "title" ] \
  && [ "$(cat "$SCRATCH/bundle-copy/body")" = "body" ]"#,
    },
    Row {
        id: "diff",
        class: RowClass::Read,
        tool: None,
        script: r#"printf 'Hello, world!' > "$SCRATCH/ref" && diff "$ROOT/hello/message" "$SCRATCH/ref""#,
    },
    Row {
        id: "cmp",
        class: RowClass::Read,
        tool: None,
        script: r#"cp "$ROOT/hello/ranged" "$SCRATCH/ranged" && printf 'abcdefghijklmnopqrstuvwxyz' > "$SCRATCH/alpha" && cmp "$SCRATCH/ranged" "$SCRATCH/alpha""#,
    },
    Row {
        id: "sha256",
        class: RowClass::Read,
        tool: None,
        script: r#"h=$( (sha256sum "$ROOT/hello/message" 2>/dev/null || shasum -a 256 "$ROOT/hello/message") | awk '{print $1}'); [ "$h" = "315f5bdb76d078c43b8ac0064e4a0164612b1fce77c869345bfc94c75894edd3" ]"#,
    },
    Row {
        id: "jq",
        class: RowClass::Read,
        tool: Some("jq"),
        script: r#"[ "$(jq -r .title "$ROOT/items/open/7/item.json")" = "Item 7" ]"#,
    },
    Row {
        id: "od",
        class: RowClass::Read,
        tool: None,
        script: r#"out=$(od -An -c "$ROOT/hello/message"); printf '%s' "$out" | grep -q H && printf '%s' "$out" | grep -q e && printf '%s' "$out" | grep -q '!'"#,
    },
    Row {
        id: "xxd",
        class: RowClass::Read,
        tool: Some("xxd"),
        // Plain dump: default xxd output groups hex in 2-byte words
        // ("4865 6c6c"), so the continuous needle needs -p.
        script: r#"xxd -p "$ROOT/hello/message" | head -n 1 | grep -q 48656c6c"#,
    },
    Row {
        id: "mmap-read",
        class: RowClass::Read,
        tool: Some("python3"),
        script: r#"python3 -c 'import mmap, os
f = open(os.environ["ROOT"] + "/hello/message", "rb")
m = mmap.mmap(f.fileno(), 0, prot=mmap.PROT_READ)
assert m[:13] == b"Hello, world!", m[:13]'"#,
    },
    Row {
        id: "read-128k",
        class: RowClass::Read,
        tool: None,
        script: r#"dd if="$ROOT/hello/large-ranged" of="$SCRATCH/chunk" bs=131072 count=2 2>/dev/null
sz=$(stat -f%z "$SCRATCH/chunk" 2>/dev/null || stat -c%s "$SCRATCH/chunk"); [ "$sz" = "262144" ]"#,
    },
    // macOS NFS sidecar probe (portable: the sidecars never exist on Linux, so
    // this passes trivially there and is a real read-only-mount probe on macOS).
    Row {
        id: "no-ds-store",
        class: RowClass::Read,
        tool: None,
        script: r#"ls "$ROOT/hello" >/dev/null 2>&1 || true
ls "$ROOT/hello/bundle" >/dev/null 2>&1 || true
! test -e "$ROOT/hello/.DS_Store" && ! test -e "$ROOT/hello/._message" && ! test -e "$ROOT/hello/bundle/.DS_Store""#,
    },
    // --- writes (expect fail today; the frontends are read-only) -----------
    Row {
        id: "append",
        class: RowClass::Write,
        tool: None,
        script: r#"echo x >> "$ROOT/hello/message""#,
    },
    Row {
        id: "truncate",
        class: RowClass::Write,
        tool: None,
        script: r#": > "$ROOT/hello/message""#,
    },
    Row {
        id: "write-rename",
        class: RowClass::Write,
        tool: None,
        script: r#"printf 'x' > "$ROOT/hello/.message.tmp" && mv "$ROOT/hello/.message.tmp" "$ROOT/hello/message""#,
    },
    Row {
        id: "unlink",
        class: RowClass::Write,
        tool: None,
        script: r#"rm "$ROOT/hello/bundle/title""#,
    },
    Row {
        id: "rename-visibility",
        class: RowClass::Write,
        tool: None,
        script: r#"mv "$ROOT/hello/bundle/title" "$ROOT/hello/bundle/title2" && ls "$ROOT/hello/bundle/title2""#,
    },
];

// ===========================================================================
// Columns
// ===========================================================================

/// Cross-frontend expectation shared by every column: `grep -r` over a paged
/// directory opens the `@next`/`@all` pagination controls its readdir listed;
/// reading `@next` drains the feed, the controls are dropped from the parent's
/// dirents, and the next open gets ENOENT, so grep exits 2. This is deliberate
/// tree design (`omnifs-engine/src/tree/synthetic.rs`,
/// `tree/resolve.rs`): the mount-root ignore files shield ignore-respecting
/// walkers (`rg`, `fd`, git), and plain `grep -r` is not one. Observed
/// identically on FUSE and NFS.
const GREP_R_PAGINATION_CONTROLS: (&str, Expect) = ("grep-r", Expect::Fail);

/// FUSE-lane expectation, observed on the fuse-in-docker lane: GNU `tail -f`
/// on `hello/live-log` delivers only the initial content (one line) and never
/// the appended bytes. The follow pump does publish growth: an out-of-band
/// `stat` grows while a handle is open (`omnifs-fuse/src/read.rs` spawns
/// `spawn_live_follow_pump` and getattr reports the grown size). But the mount
/// fires no inotify events for that out-of-band growth, and GNU tail's Linux
/// default follow mode is inotify, so tail blocks forever after the initial
/// flush; explicit stat-polling probes (`tail ---disable-inotify`, with
/// `stdbuf -o0`) also captured nothing in 5s. The same fixture delivers growth
/// to BSD tail over the NFS lane, so this is a FUSE-follow gap; the pump's own
/// design comment targets a polling `tail -f`, so this row flipping green is
/// the signal that the gap is closed.
const TAIL_F_FUSE_FOLLOW_GAP: (&str, Expect) = ("tail-f-growing", Expect::Fail);

/// Linux kernel-FUSE native lane. Seeded from the fuse-in-docker observations
/// (same frontend); CI validates.
pub const LINUX_FUSE_NATIVE: Column = Column {
    id: "linux-fuse-native",
    platform: "linux",
    expectations: &[GREP_R_PAGINATION_CONTROLS, TAIL_F_FUSE_FOLLOW_GAP],
};

/// macOS NFSv4.0 loopback lane.
pub const MACOS_NFS_LOOPBACK: Column = Column {
    id: "macos-nfs-loopback",
    platform: "macos",
    expectations: &[
        GREP_R_PAGINATION_CONTROLS,
        // bsdtar stats each file before reading and writes the archive header
        // from that size; a cold stat on this lane reports the not-yet-learned
        // size sentinel (1), so the archive truncates content to one byte.
        // Catalogued in docs/architecture/10-file-attributes.md: `tar c` is in
        // the table of tools that need exact size, and the host serves the
        // smallest useful sentinel until a read learns the length. The FUSE
        // lanes pass this row, so the manifestation is NFS-attr-path-specific.
        ("tar", Expect::Fail),
    ],
};

/// FUSE-in-the-product-container lane. Same kernel FUSE frontend as the native
/// Linux lane.
pub const FUSE_IN_DOCKER: Column = Column {
    id: "fuse-in-docker",
    platform: "linux",
    expectations: &[GREP_R_PAGINATION_CONTROLS, TAIL_F_FUSE_FOLLOW_GAP],
};
