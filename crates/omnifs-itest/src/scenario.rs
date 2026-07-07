//! Data-driven provider scenarios over the callout tape system.
//!
//! A [`Scenario`] is data: a mount config, an ordered list of [`Step`]s, and the
//! recording rules. [`run`] executes it in one of two modes selected by the
//! `OMNIFS_TAPE` env var, and the scenario code path is identical in both
//! (invariant I5); only how callouts are answered differs. In replay (the
//! default, hermetic host-test lane) captured callouts are answered from the
//! checked-in tape; in record mode real executors run against live upstreams
//! while a [`TapeRecorder`] tees every exchange to disk.
//!
//! Each step renders a deterministic [`StepTrace`] (projection entries, read
//! bytes, and the terminal effects) that is snapshotted with `insta`. The trace
//! is deterministic by construction: entries and effects are sorted, and no
//! timestamps, runtime ids, or map iteration order ever reach the rendering.

use std::path::{Path as StdPath, PathBuf};

use omnifs_engine::test_support::TestOp;
use omnifs_engine::test_support::blob::BlobCache;
use omnifs_wit::provider::types as wit;

use crate::tape::record::TapeRecorder;
use crate::tape::replay::TapePlayer;
use crate::tape::scrub::TapeRules;
use crate::tape::{TapeError, sha256_hex};
use crate::{CalloutSetup, RuntimeHarness};

/// One data-driven provider scenario: a mount config plus an ordered list of
/// filesystem operations, recorded once and replayed hermetically thereafter.
pub struct Scenario {
    /// Kebab-case; becomes the tape filename and the snapshot-name prefix.
    pub name: &'static str,
    /// The provider test directory (e.g. `"github"`). Tapes and snapshots live
    /// under `tests/<dir>/`, next to the scenario's `#[test]` fns.
    pub dir: &'static str,
    /// Mount config JSON, the same shape the harness takes today.
    pub config: &'static str,
    /// `None` for unauthenticated providers (dns, web, arxiv public APIs).
    pub auth: Option<RecordAuth>,
    pub rules: TapeRules,
    /// Local fixture setup (e.g. seeding a repo clone cache). Runs in BOTH
    /// modes after harness construction and before the first step.
    pub setup: Option<fn(&RuntimeHarness)>,
    pub steps: &'static [Step],
}

/// How record mode obtains a credential for an authenticated scenario's mount.
pub struct RecordAuth {
    /// Env var holding the token, e.g. `"OMNIFS_RECORD_GITHUB_TOKEN"`.
    pub token_env: &'static str,
}

/// One filesystem operation in a scenario.
pub enum Step {
    /// `list-children` at a path; the trace records the returned entries.
    List(&'static str),
    /// `read-file` at a path; the trace records attrs, bytes, and effects.
    Read(&'static str),
    /// Revalidating `read-file` at a path: the cached canonical is pushed back
    /// with `revalidate: true` (the engine's background-revalidation op shape),
    /// so the provider issues a conditional fetch against the stored validator.
    /// Requires a prior step to have cached the path's object canonical.
    Revalidate(&'static str),
    /// `lookup-child`; the trace records the outcome.
    Lookup {
        parent: &'static str,
        name: &'static str,
    },
    /// Fire `on-event(timer-tick)`; the trace records the resulting effects.
    TimerTick,
}

/// The record/replay dispatch selected from `OMNIFS_TAPE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Replay,
    Record,
}

/// Execute a scenario in the mode selected by `OMNIFS_TAPE` (unset or `"replay"`
/// replays the checked-in tape; `"record"` records against live upstreams). Any
/// other value is a hard error. Called from a provider's `#[test]` fns.
///
/// # Panics
///
/// Panics on an unknown `OMNIFS_TAPE` value, a tape load/miss, a divergent
/// snapshot, or (in record mode) a missing auth env var or a recording that
/// does not replay.
pub fn run(scenario: &Scenario) {
    let mode = mode_from_env(std::env::var("OMNIFS_TAPE").ok().as_deref())
        .unwrap_or_else(|message| panic!("{message}"));
    match mode {
        Mode::Replay => run_replay(scenario),
        Mode::Record => run_record(scenario),
    }
}

/// Parse the mode from the raw `OMNIFS_TAPE` value. Pure; unit-tested.
fn mode_from_env(raw: Option<&str>) -> Result<Mode, String> {
    match raw {
        None | Some("replay") => Ok(Mode::Replay),
        Some("record") => Ok(Mode::Record),
        Some(other) => Err(format!(
            "OMNIFS_TAPE must be unset, \"replay\", or \"record\"; got {other:?}. \
             Use `just host itest-record <provider> [scenario]` to record."
        )),
    }
}

fn run_replay(scenario: &Scenario) {
    let harness = RuntimeHarness::builder(scenario.config)
        .callouts(CalloutSetup::Captured)
        .build()
        .expect("build replay harness");
    let tape_path = tape_path(scenario);
    let mut player = TapePlayer::load(&tape_path)
        .unwrap_or_else(|error| panic!("load tape {}: {error}", tape_path.display()));

    if let Some(setup) = scenario.setup {
        setup(&harness);
    }

    let settings = snapshot_settings(scenario);
    settings.bind(|| {
        for (index, step) in scenario.steps.iter().enumerate() {
            let trace = replay_step_trace(&harness, &mut player, index, step);
            insta::assert_snapshot!(snapshot_name(scenario.name, index, step), trace);
        }
    });
}

fn run_record(scenario: &Scenario) {
    // Resolve the credential the mount will authenticate with. The prepare hook
    // writes it into `paths.credentials_file` via `omnifs_workspace::creds` so
    // the engine's credential service reads exactly what the CLI writer stores.
    let secrets = record_secrets(scenario);
    let credential = scenario
        .auth
        .as_ref()
        .map(|auth| record_credential(scenario, auth));

    let recorder = TapeRecorder::new();
    let mut builder =
        RuntimeHarness::builder(scenario.config).callouts(CalloutSetup::Recorded(recorder.clone()));
    if let Some((credential_id, token)) = credential {
        builder = builder.prepare(move |layout| {
            write_static_credential(&layout.credentials_file, &credential_id, &token);
        });
    }
    let harness = builder.build().expect("build record harness");

    if let Some(setup) = scenario.setup {
        setup(&harness);
    }

    // Record mode runs real executors, so each op returns without a callout
    // burst to answer. Snapshots regenerate here because the recipe sets
    // INSTA_UPDATE=always; the rendered traces are kept for record-then-verify.
    let settings = snapshot_settings(scenario);
    let recorded_traces = settings.bind(|| {
        scenario
            .steps
            .iter()
            .enumerate()
            .map(|(index, step)| {
                let trace = record_step_trace(&harness, index, step);
                let name = snapshot_name(scenario.name, index, step);
                insta::assert_snapshot!(name.clone(), trace.clone());
                trace
            })
            .collect::<Vec<_>>()
    });

    let tape_path = tape_path(scenario);
    let sidecar_dir = sidecar_dir(scenario);
    let blob_cache = harness.runtime.blob_cache_for_tests().clone();
    recorder
        .finalize(
            &scenario.rules,
            &tape_path,
            &sidecar_dir,
            |id| blob_cache.bytes_for_tests(id),
            &secrets,
        )
        .unwrap_or_else(|error| panic!("finalize tape {}: {error}", tape_path.display()));

    // Record-then-verify: a recording that does not replay is not a recording.
    // The fresh replay must reproduce byte-identical traces; a divergence fails
    // the recording. The comparison is a direct string equality rather than an
    // insta assertion, because INSTA_UPDATE=always is in force for record mode
    // and would otherwise silently overwrite the snapshot with a divergent
    // replay instead of failing.
    verify_replay(scenario, &recorded_traces);
}

/// Replay the just-written tape and require byte-identical traces, else fail.
fn verify_replay(scenario: &Scenario, recorded: &[String]) {
    let harness = RuntimeHarness::builder(scenario.config)
        .callouts(CalloutSetup::Captured)
        .build()
        .expect("build record-then-verify harness");
    let tape_path = tape_path(scenario);
    let mut player = TapePlayer::load(&tape_path)
        .unwrap_or_else(|error| panic!("record-then-verify load {}: {error}", tape_path.display()));

    if let Some(setup) = scenario.setup {
        setup(&harness);
    }

    for (index, step) in scenario.steps.iter().enumerate() {
        let replayed = replay_step_trace(&harness, &mut player, index, step);
        let expected = &recorded[index];
        assert!(
            &replayed == expected,
            "record-then-verify: replay of scenario {} step {index} diverged from the \
             recording. A recording that does not replay is not a recording.\n\
             --- recorded ---\n{expected}\n--- replayed ---\n{replayed}\n",
            scenario.name,
        );
    }
}

// --- step driving ---

/// Build the [`TestOp`] for a step through the harness op helpers.
fn build_op<'a>(harness: &'a RuntimeHarness, step: &Step) -> TestOp<'a> {
    match step {
        Step::List(path) => harness.list(path),
        Step::Read(path) => harness.read(path),
        Step::Revalidate(path) => harness.revalidate(path),
        Step::Lookup { parent, name } => harness.lookup(parent, name),
        Step::TimerTick => harness.timer_tick(),
    }
    .expect("start scenario op")
}

/// Drive a captured op to completion, answering each callout burst from the
/// tape, then render its trace.
fn replay_step_trace(
    harness: &RuntimeHarness,
    player: &mut TapePlayer,
    index: usize,
    step: &Step,
) -> String {
    let mut op = build_op(harness, step);
    let blobs = harness.runtime.blob_cache_for_tests().clone();
    drive(&mut op, player, blobs.as_ref()).unwrap_or_else(|error| panic!("{error}"));
    trace_returned(index, step, &op, blobs.as_ref())
}

/// Render a record-mode op. Real executors already ran it to completion, so
/// there is no callout burst to answer.
fn record_step_trace(harness: &RuntimeHarness, index: usize, step: &Step) -> String {
    let op = build_op(harness, step);
    let blobs = harness.runtime.blob_cache_for_tests().clone();
    trace_returned(index, step, &op, blobs.as_ref())
}

/// The plan's drive loop: while the op is parked on captured callouts, answer
/// each pending callout from the tape and resume. A tape miss surfaces its
/// rendered report as a [`TapeError`].
fn drive(op: &mut TestOp<'_>, player: &mut TapePlayer, blobs: &BlobCache) -> Result<(), TapeError> {
    while op.is_waiting_for_callouts() {
        let answers = op
            .callouts()
            .iter()
            .map(|callout| player.answer(callout, blobs))
            .collect::<Result<Vec<_>, _>>()?;
        op.answer_callouts(answers)
            .expect("answer captured callouts");
    }
    Ok(())
}

fn trace_returned(index: usize, step: &Step, op: &TestOp<'_>, blobs: &BlobCache) -> String {
    let result = op
        .result()
        .unwrap_or_else(|| panic!("scenario step {index} did not return"))
        .clone();
    let effects = op
        .effects()
        .cloned()
        .expect("a returned op carries effects");
    render_step(index, step, &result, &effects, blobs)
}

// --- trace rendering ---

const READ_BODY_INLINE_MAX: usize = 8192; // 8 KiB

/// Render one step into a deterministic text block: a header, the operation's
/// projection body, and the terminal effects. Sorting everywhere keeps the
/// output independent of map iteration order.
fn render_step(
    index: usize,
    step: &Step,
    result: &wit::OpResult,
    effects: &wit::Effects,
    blobs: &BlobCache,
) -> String {
    let mut lines = vec![format!("## step {index}: {}", op_description(step))];
    match step {
        Step::List(_) => render_list(result, &mut lines),
        Step::Read(path) => render_read(result, effects, path, blobs, &mut lines),
        Step::Revalidate(path) => render_revalidate(result, effects, path, blobs, &mut lines),
        Step::Lookup { .. } => render_lookup(result, &mut lines),
        Step::TimerTick => lines.push("on-event".to_owned()),
    }
    render_effects(effects, &mut lines);
    lines.join("\n")
}

fn op_description(step: &Step) -> String {
    match step {
        Step::List(path) => format!("list {path}"),
        Step::Read(path) => format!("read {path}"),
        Step::Revalidate(path) => format!("revalidate {path}"),
        Step::Lookup { parent, name } => format!("lookup {parent} :: {name}"),
        Step::TimerTick => "timer-tick".to_owned(),
    }
}

fn render_list(result: &wit::OpResult, lines: &mut Vec<String>) {
    match result {
        wit::OpResult::ListChildren(wit::ListChildrenResult::Entries(listing)) => {
            let mut entries: Vec<String> = listing.entries.iter().map(render_entry_line).collect();
            entries.sort();
            lines.extend(entries);
        },
        wit::OpResult::ListChildren(wit::ListChildrenResult::Subtree(_)) => {
            lines.push("subtree".to_owned());
        },
        wit::OpResult::ListChildren(wit::ListChildrenResult::Unchanged) => {
            lines.push("unchanged".to_owned());
        },
        other => lines.push(format!("unexpected list result: {other:?}")),
    }
}

/// `{name}  {kind}  {size-or-dash}  {stability}` for one directory entry.
fn render_entry_line(entry: &wit::DirEntry) -> String {
    let (kind, size, stability) = match &entry.kind {
        wit::EntryKind::Directory => ("dir", "-".to_owned(), "-"),
        wit::EntryKind::File(file) => (
            "file",
            render_size(&file.attrs.size),
            stability_str(file.attrs.stability),
        ),
    };
    format!("{}  {kind}  {size}  {stability}", entry.name)
}

fn render_read(
    result: &wit::OpResult,
    effects: &wit::Effects,
    path: &str,
    blobs: &BlobCache,
    lines: &mut Vec<String>,
) {
    match result {
        wit::OpResult::ReadFile(wit::ReadFileOutcome::Found(file)) => {
            let stability = stability_str(file.attrs.stability);
            let version = file.attrs.version_token.as_deref().unwrap_or("-");
            let size = render_size(&file.attrs.size);
            lines.push(format!("attrs: {stability} {version} {size}"));
            let bytes = read_bytes(&file.bytes, effects, path, blobs);
            lines.push(render_body(&bytes));
        },
        wit::OpResult::ReadFile(wit::ReadFileOutcome::NotFound(_)) => {
            lines.push("not-found".to_owned());
        },
        other => lines.push(format!("unexpected read result: {other:?}")),
    }
}

/// Render a revalidating read. An unchanged revalidation (the provider's
/// conditional fetch matched the validator) serves `byte-source::canonical`
/// with no new canonical effects, so the trace states the unchanged outcome
/// instead of resolving bytes from the (empty) effects. A fresh reload (the
/// validator no longer matched upstream) re-stores the canonical and renders
/// exactly like a read.
fn render_revalidate(
    result: &wit::OpResult,
    effects: &wit::Effects,
    path: &str,
    blobs: &BlobCache,
    lines: &mut Vec<String>,
) {
    match result {
        wit::OpResult::ReadFile(wit::ReadFileOutcome::Found(file))
            if matches!(file.bytes, wit::ByteSource::Canonical) && effects.canonical.is_empty() =>
        {
            let stability = stability_str(file.attrs.stability);
            let version = file.attrs.version_token.as_deref().unwrap_or("-");
            let size = render_size(&file.attrs.size);
            lines.push(format!("attrs: {stability} {version} {size}"));
            lines.push("bytes: unchanged (validator matched, served from warm canonical)".into());
        },
        other => render_read(other, effects, path, blobs, lines),
    }
}

/// Resolve the bytes a read answered with. `canonical` references the canonical
/// store for this path; `blob` lives in the runtime blob cache; `inline` is
/// direct. `deferred` is never a valid read answer.
fn read_bytes(
    source: &wit::ByteSource,
    effects: &wit::Effects,
    path: &str,
    blobs: &BlobCache,
) -> Vec<u8> {
    match source {
        wit::ByteSource::Inline(bytes) => bytes.clone(),
        wit::ByteSource::Canonical => effects
            .canonical
            .iter()
            .find(|store| store.view_leaves.iter().any(|leaf| leaf == path))
            .or_else(|| effects.canonical.first())
            .map(|store| store.bytes.clone())
            .unwrap_or_default(),
        wit::ByteSource::Blob(id) => blobs.bytes_for_tests(*id).unwrap_or_default(),
        wit::ByteSource::Deferred(_) => Vec::new(),
    }
}

/// `bytes ({n}):` then the body verbatim when it is `UTF-8` and small enough to
/// review inline, otherwise a stable digest line.
fn render_body(bytes: &[u8]) -> String {
    let count = bytes.len();
    if count <= READ_BODY_INLINE_MAX
        && let Ok(text) = std::str::from_utf8(bytes)
    {
        format!("bytes ({count}):\n{text}")
    } else {
        format!(
            "bytes ({count}):\nsha256:{} ({count} bytes)",
            sha256_hex(bytes)
        )
    }
}

fn render_lookup(result: &wit::OpResult, lines: &mut Vec<String>) {
    // The rendered outcome is the wit `LookupChildResult` (what `op.result()`
    // returns), not the host-materialized `effects::apply::LookupOutcome` the
    // plan named: the trace works from the wire result, before materialization.
    match result {
        wit::OpResult::LookupChild(wit::LookupChildResult::Entry(entry)) => {
            lines.push(format!(
                "entry: {} {}",
                entry.target.name,
                entry_kind_str(&entry.target.kind)
            ));
        },
        wit::OpResult::LookupChild(wit::LookupChildResult::Subtree(_)) => {
            lines.push("subtree".to_owned());
        },
        wit::OpResult::LookupChild(wit::LookupChildResult::NotFound(_)) => {
            lines.push("not-found".to_owned());
        },
        other => lines.push(format!("unexpected lookup result: {other:?}")),
    }
}

/// Render the terminal effects: canonical stores keyed by their view leaves,
/// filesystem writes, and invalidations. Every block is sorted so the trace is
/// insensitive to the order the provider emitted them in.
fn render_effects(effects: &wit::Effects, lines: &mut Vec<String>) {
    lines.push("canonical:".to_owned());
    let mut canonical = Vec::new();
    for store in &effects.canonical {
        let digest = sha256_hex(&store.bytes);
        for leaf in &store.view_leaves {
            canonical.push(format!("  {leaf}  sha256:{digest}"));
        }
    }
    push_sorted(lines, canonical);

    lines.push("fs:".to_owned());
    let fs = effects
        .fs
        .iter()
        .map(|write| format!("  {}  {}", write.path, render_fs_kind(&write.kind)))
        .collect();
    push_sorted(lines, fs);

    lines.push("invalidations:".to_owned());
    let invalidations = effects
        .invalidations
        .iter()
        .map(render_invalidation)
        .collect();
    push_sorted(lines, invalidations);
}

fn push_sorted(lines: &mut Vec<String>, mut block: Vec<String>) {
    if block.is_empty() {
        lines.push("  (none)".to_owned());
        return;
    }
    block.sort();
    lines.extend(block);
}

fn render_fs_kind(kind: &wit::FsKind) -> String {
    match kind {
        wit::FsKind::Directory(exhaustive) => format!("dir(exhaustive={exhaustive})"),
        wit::FsKind::File(file) => format!(
            "file {} {} {} {}",
            stability_str(file.attrs.stability),
            file.attrs.version_token.as_deref().unwrap_or("-"),
            render_size(&file.attrs.size),
            byte_source_kind(&file.bytes),
        ),
    }
}

fn render_invalidation(invalidation: &wit::Invalidation) -> String {
    match invalidation {
        wit::Invalidation::Object(id) => format!("  object {}", render_logical_id(id)),
        wit::Invalidation::Listing(wit::PathOrPrefix::Path(path)) => {
            format!("  listing path {path}")
        },
        wit::Invalidation::Listing(wit::PathOrPrefix::Prefix(prefix)) => {
            format!("  listing prefix {prefix}")
        },
    }
}

/// `{kind}[{name}={value},...]` with captures sorted by name. Renders only the
/// provider-supplied logical identity, never a runtime id.
fn render_logical_id(id: &wit::LogicalId) -> String {
    let mut captures: Vec<String> = id
        .captures
        .iter()
        .map(|capture| format!("{}={}", capture.name, capture.value))
        .collect();
    captures.sort();
    format!("{}[{}]", id.kind, captures.join(","))
}

fn render_size(size: &wit::FileSize) -> String {
    match size {
        wit::FileSize::Exact(bytes) => bytes.to_string(),
        wit::FileSize::NonZero => "non-zero".to_owned(),
        wit::FileSize::Unknown => "unknown".to_owned(),
    }
}

fn stability_str(stability: wit::Stability) -> &'static str {
    match stability {
        wit::Stability::Stable => "stable",
        wit::Stability::Dynamic => "dynamic",
        wit::Stability::Live => "live",
    }
}

fn entry_kind_str(kind: &wit::EntryKind) -> &'static str {
    match kind {
        wit::EntryKind::Directory => "dir",
        wit::EntryKind::File(_) => "file",
    }
}

/// The byte-source discriminant only. A blob id and inline bytes are elided:
/// the id is runtime-local and the bytes render under `read`/`canonical`.
fn byte_source_kind(source: &wit::ByteSource) -> String {
    match source {
        wit::ByteSource::Inline(_) => "inline".to_owned(),
        wit::ByteSource::Canonical => "canonical".to_owned(),
        wit::ByteSource::Blob(_) => "blob".to_owned(),
        wit::ByteSource::Deferred(mode) => format!("deferred({})", read_mode_str(*mode)),
    }
}

fn read_mode_str(mode: wit::ReadMode) -> &'static str {
    match mode {
        wit::ReadMode::Full => "full",
        wit::ReadMode::Ranged => "ranged",
    }
}

// --- snapshot naming ---

/// `{scenario}__{index:02}-{step-label}`, the explicit snapshot name.
fn snapshot_name(scenario_name: &str, index: usize, step: &Step) -> String {
    format!("{scenario_name}__{index:02}-{}", step_label(step))
}

/// The step kind plus the last path segment, sanitized to `[a-z0-9-]`.
fn step_label(step: &Step) -> String {
    match step {
        Step::List(path) => format!("list-{}", last_segment(path)),
        Step::Read(path) => format!("read-{}", last_segment(path)),
        Step::Revalidate(path) => format!("revalidate-{}", last_segment(path)),
        Step::Lookup { name, .. } => format!("lookup-{}", sanitize(name)),
        Step::TimerTick => "timer-tick".to_owned(),
    }
}

/// The last non-empty path segment sanitized, or `"root"` for `/`.
fn last_segment(path: &str) -> String {
    let segment = path.rsplit('/').find(|part| !part.is_empty());
    match segment.map(sanitize) {
        Some(label) if !label.is_empty() => label,
        _ => "root".to_owned(),
    }
}

/// Lowercase, collapse every run of non-`[a-z0-9]` characters to a single `-`,
/// and trim leading/trailing `-`.
fn sanitize(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut pending_dash = false;
    for ch in input.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(lower);
        } else {
            pending_dash = true;
        }
    }
    out
}

// --- paths and settings ---

fn manifest_dir() -> &'static StdPath {
    StdPath::new(env!("CARGO_MANIFEST_DIR"))
}

fn tape_path(scenario: &Scenario) -> PathBuf {
    manifest_dir()
        .join("tests")
        .join(scenario.dir)
        .join("tapes")
        .join(format!("{}.jsonl", scenario.name))
}

fn sidecar_dir(scenario: &Scenario) -> PathBuf {
    manifest_dir()
        .join("tests")
        .join(scenario.dir)
        .join("tapes")
        .join("blobs")
}

fn snapshots_dir(scenario: &Scenario) -> PathBuf {
    manifest_dir()
        .join("tests")
        .join(scenario.dir)
        .join("snapshots")
}

/// Bind snapshots to the scenario's `tests/<dir>/snapshots` directory with no
/// module prefix, so the pilot's snapshots live next to its tapes and the
/// explicit `{scenario}__{index}-{label}` name is the whole file stem.
///
/// insta resolves a snapshot file as `workspace / assertion_file.parent() /
/// snapshot_path / name.snap`; an absolute `snapshot_path` overrides the prefix
/// entirely (verified against insta 1.48 `runtime::get_snapshot_filename`).
fn snapshot_settings(scenario: &Scenario) -> insta::Settings {
    let mut settings = insta::Settings::clone_current();
    settings.set_snapshot_path(snapshots_dir(scenario));
    settings.set_prepend_module_to_snapshot(false);
    settings
}

// --- record-mode credential seeding ---

/// The secret strings the tripwire scans for after writing the tape.
fn record_secrets(scenario: &Scenario) -> Vec<String> {
    scenario
        .auth
        .as_ref()
        .map(|auth| vec![record_token(auth)])
        .unwrap_or_default()
}

/// Read the record token from the scenario's auth env var.
fn record_token(auth: &RecordAuth) -> String {
    std::env::var(auth.token_env).unwrap_or_else(|_| {
        panic!(
            "record mode requires the {} environment variable to hold a valid upstream token",
            auth.token_env
        )
    })
}

/// Derive the credential id the engine will look up for this mount, mirroring
/// the CLI's `CredentialId::for_mount` keying, and pair it with the token.
fn record_credential(
    scenario: &Scenario,
    auth: &RecordAuth,
) -> (omnifs_workspace::authn::CredentialId, String) {
    let token = record_token(auth);
    let credential_id = credential_id_for_config(scenario.config);
    (credential_id, token)
}

/// Compute the mount's credential id from its config: the provider NAME slug
/// (read from the pinned artifact's manifest, like the harness does) plus the
/// config's declared auth scheme and account.
fn credential_id_for_config(config: &str) -> omnifs_workspace::authn::CredentialId {
    use omnifs_workspace::mounts::Auth;
    use omnifs_workspace::provider::Artifact;

    let value: serde_json::Value =
        serde_json::from_str(config).expect("parse scenario mount config");
    let provider_file = value
        .get("provider")
        .and_then(serde_json::Value::as_str)
        .expect("scenario config has a string `provider`");

    let src = crate::provider_wasm_path(provider_file);
    let bytes = std::fs::read(&src).expect("read provider wasm for credential keying");
    let artifact = Artifact::from_bytes(provider_file, bytes).expect("build provider artifact");
    let provider_name = artifact.meta().name.clone();

    let auth: Auth = serde_json::from_value(
        value
            .get("auth")
            .cloned()
            .expect("an authenticated scenario config declares an `auth` block"),
    )
    .expect("parse scenario auth block");
    let scheme = auth
        .scheme()
        .expect("an authenticated record scenario must declare its auth scheme");
    omnifs_workspace::authn::CredentialId::for_mount(&provider_name, &auth, scheme)
        .expect("derive credential id")
}

/// Write a static-token credential into the store the engine reads, exactly as
/// the CLI's static-token import does (`CredentialEntry::static_token` + store
/// `put`), so the mount authenticates during recording.
fn write_static_credential(
    credentials_file: &StdPath,
    credential_id: &omnifs_workspace::authn::CredentialId,
    token: &str,
) {
    use omnifs_workspace::creds::{CredentialEntry, CredentialStore, FileStore};

    let store = FileStore::new(credentials_file);
    let entry = CredentialEntry::static_token(
        secrecy::SecretString::from(token.to_owned()),
        time::OffsetDateTime::now_utc(),
    );
    store
        .put(credential_id, &entry)
        .expect("write record-mode credential");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_defaults_to_replay() {
        assert_eq!(mode_from_env(None), Ok(Mode::Replay));
        assert_eq!(mode_from_env(Some("replay")), Ok(Mode::Replay));
    }

    #[test]
    fn mode_record_is_recognized() {
        assert_eq!(mode_from_env(Some("record")), Ok(Mode::Record));
    }

    #[test]
    fn mode_rejects_unknown_values() {
        let err = mode_from_env(Some("replayy")).expect_err("unknown value must error");
        assert!(err.contains("OMNIFS_TAPE"));
        assert!(err.contains("replayy"));
    }

    #[test]
    fn sanitize_lowercases_and_collapses_separators() {
        assert_eq!(sanitize("README.md"), "readme-md");
        assert_eq!(sanitize("0xff-ai"), "0xff-ai");
        assert_eq!(sanitize("daily__sleep.json"), "daily-sleep-json");
        assert_eq!(sanitize("A B/C"), "a-b-c");
        // Leading/trailing separators are trimmed.
        assert_eq!(sanitize("/leading"), "leading");
        assert_eq!(sanitize("trailing/"), "trailing");
    }

    #[test]
    fn last_segment_uses_trailing_path_component() {
        assert_eq!(last_segment("/0xff-ai/omnifs/README.md"), "readme-md");
        assert_eq!(last_segment("/0xff-ai"), "0xff-ai");
        // Root and trailing-slash paths fall back to "root".
        assert_eq!(last_segment("/"), "root");
        assert_eq!(last_segment(""), "root");
        assert_eq!(last_segment("/repos/"), "repos");
    }

    #[test]
    fn snapshot_names_are_zero_padded_and_labeled() {
        assert_eq!(
            snapshot_name("repo-browse", 0, &Step::List("/")),
            "repo-browse__00-list-root"
        );
        assert_eq!(
            snapshot_name("repo-browse", 3, &Step::Read("/0xff-ai/omnifs/README.md")),
            "repo-browse__03-read-readme-md"
        );
        assert_eq!(
            snapshot_name(
                "repo-browse",
                12,
                &Step::Lookup {
                    parent: "/",
                    name: "omnifs"
                }
            ),
            "repo-browse__12-lookup-omnifs"
        );
        assert_eq!(
            snapshot_name("revalidation", 1, &Step::TimerTick),
            "revalidation__01-timer-tick"
        );
        assert_eq!(
            snapshot_name(
                "revalidation",
                1,
                &Step::Revalidate("/octocat/Hello-World/repo.json")
            ),
            "revalidation__01-revalidate-repo-json"
        );
    }
}
