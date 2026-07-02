# omnifs unified execution plan

Status: the single execution document for the omnifs rework and strategy build-out. It unifies `docs/future/bluesky-rewrite.md` (architecture), `docs/future/bluesky-tree.md` (target layout), `docs/future/cli-experience.md` (CLI UX), and `docs/internal/strategy/15-status-2026-07.md` (strategy gaps). Those documents carry design rationale; **this document carries the execution truth**. If this document and a companion disagree on sequencing or acceptance, this document wins. Every step below is self-contained: it names its files, its exact changes, its verification commands, and its expected outcomes.

---

## 0. Executor contract

You are an agent executing this plan autonomously. These rules are binding and have no exceptions:

### 0.1 Ordering and parallelism

- Execute steps in the order of the status ledger (§1) unless a step is marked `[PARALLEL-OK]`, which means it may be executed at any point after its listed dependencies are done.
- Never start a step whose `Depends on` entries are not all `done` or `pr-open` (stacking rules in 0.3).
- Steps marked `[HORIZON]` must NOT be executed; they exist to mark the boundary of this plan.

### 0.2 Stop conditions (halt and report to Raul; do not improvise)

1. A step is marked `[GATED]`: do the work, open the PR, set ledger status `blocked-gated`, and stop that track. Never merge or build on top of a gated step without explicit approval.
2. A gate command fails, you make ONE focused fix attempt, and it fails again: stop, set status `blocked-failed`, report the exact command output.
3. A step marked `[LINUX]` and no Linux build host is available: stop that step with status `blocked-linux`. Never report a vacuous macOS compile as a pass; FUSE and `/proc/mounts` code is `cfg(target_os = "linux")` and does not compile on macOS.
4. A step marked `[COST]` (spends money on model API calls): request approval before running.
5. Completing a step would require weakening any invariant in `docs/future/bluesky-rewrite.md` §"Merge-blocking bars" or the "Universal invariants" of AGENTS.md: stop and surface the conflict.
6. Newly inspected code contradicts a factual claim in a step (a file moved, a symbol renamed): re-verify against the current tree, adapt mechanically if the intent is unambiguous, otherwise stop with status `blocked-drift`.

### 0.3 Branch, commit, and PR rules

- One step = one PR. Branch name: `bluesky/<step-id>-<slug>` (example: `bluesky/M2-workspace-merge`).
- Branch from `main` if all dependencies are merged; otherwise branch from the dependency's branch and say so in the PR body ("Stacked on #NNN").
- Commit messages: conventional commits (`<type>(<scope>): <imperative>`); body explains why. Never append a Claude-Session trailer.
- PR titles are given per step. PR bodies: 2-3 sentence lede naming the concept that changed, then `##` sections with bolded-lead bullets. No test-plan sections. Single line per paragraph (no hard wrapping).
- After opening a PR, update the status ledger in THIS file (status + PR number) and include that ledger edit in the PR.

### 0.4 Gate conventions

- Run every gate exactly as written and check the exit code. Never pipe a gate through `tail`, `head`, or `grep` in a way that masks the exit code; when output must be filtered, use `cmd > /tmp/gate.log 2>&1; echo exit=$?` and then inspect the log.
- The standing gates, referenced by name below:
  - `GATE-FMT`: `cargo fmt --check; echo exit=$?` → `exit=0`
  - `GATE-TEST`: `cargo nextest run; echo exit=$?` → `exit=0`
  - `GATE-HOST`: `just host clippy && just host test; echo exit=$?` → `exit=0` (never use `cargo check --workspace --all-targets`; it force-compiles guest crates for the host target and fails on main)
  - `GATE-PROVIDERS`: `RUSTC_WRAPPER= just providers build; echo exit=$?` → `exit=0` (run `just providers wasi-sdk` first on a fresh machine)
  - `GATE-OPENAPI`: `just openapi && git diff --exit-code crates/omnifs-api/openapi/daemon.json; echo exit=$?` → `exit=0` after regeneration is committed
  - `GATE-SCHEMA`: `just schema && git diff --exit-code; echo exit=$?` → `exit=0` (only for manifest-schema-touching steps)
  - `GATE-DOCS`: `bash scripts/ci/check-doc-links.sh 2>&1 | grep -c "bluesky\|cli-experience\|execution"` → `0` (repo-wide docs-check has pre-existing failures in docs/internal; your gate is only that YOUR files introduce no dangling links)
  - `GATE-LIVE`: `just dev -y` then `docker exec omnifs /bin/zsh -lc 'omnifs status'; echo exit=$?` → `exit=0`, and `docker exec omnifs /bin/zsh -lc 'ls /omnifs && cat $(find /omnifs -maxdepth 3 -type f | head -1) > /dev/null'; echo exit=$?` → `exit=0`
- Provider-test interaction footgun: when running host integration tests after building providers, use `OMNIFS_ITEST_SKIP_PROVIDER_BUILD=1 cargo nextest run ...` or `just host test` (which sets it).
- After `.wit` edits, run `cargo clean -p omnifs-wit` before trusting downstream errors.

### 0.5 Scope discipline

- Do exactly what the step says. If you notice an adjacent improvement, note it in the PR body under "Out of scope, observed" and do not do it.
- Co-edit producers with consumers in one pass: when a signature or type moves, update every call site in the same commit series, never leaving the tree uncompilable at a step boundary you hand off.
- Never leave any provider under `providers/` broken at a PR boundary.

### 0.6 Session recovery

A fresh session resumes by: (1) reading this file top to bottom, (2) finding the first ledger row whose status is `todo` or `in-progress`, (3) verifying that row's dependencies are satisfied, (4) verifying the row's preconditions against the actual tree (0.2 rule 6), (5) continuing.

---

## 1. Status ledger

Update this table as you work. Status values: `todo`, `in-progress`, `pr-open (#N)`, `blocked-gated`, `blocked-failed`, `blocked-linux`, `blocked-drift`, `done`.

| Step | Title | Flags | Depends on | Status |
|---|---|---|---|---|
| PF | Preflight | — | — | todo |
| T1 | Latency measurement suite | PARALLEL-OK | PF | todo |
| T2 | Dogfood telemetry counters | PARALLEL-OK | PF | todo |
| T3 | Benchmark harness (deterministic) | PARALLEL-OK | PF | todo |
| T4 | Live benchmark + M0 report | COST, GATED | T1, T3 | todo |
| N1 | Land the phase-a deletions branch | — | PF | todo |
| N2 | Characterization test net | — | N1 | todo |
| N3 | Frontend concurrency net | — | N2 | todo |
| M1 | omnifs-api absorbs omnifs-inspector | — | N2 | todo |
| M2 | Extract omnifs-mtab | LINUX | N2 | todo |
| M3 | omnifs-workspace merge | GATED(utoipa) | M1 | todo |
| M4 | omnifs-engine merge + visibility | — | M3 | todo |
| M5 | omnifs-auth retarget | — | M3 | todo |
| U1 | Naming batch 1 (macOS-safe) | — | M4 | todo |
| U2 | Naming batch 2 (Backing→Subtree) | LINUX | M4 | todo |
| U3 | Strict top-level Spec parsing | GATED | M3 | todo |
| U4 | Need → AccessNeed/ResourceLimit split | — | M3, M4 | todo |
| A1 | CredentialService core | — | M4, M5, N2 | todo |
| A2 | Proactive refresh loop + health | — | A1 | todo |
| A3 | Rejection backstop + NeedsConsent | — | A2 | todo |
| A4 | Revocation wiring | — | A1 | todo |
| A5 | device_poll_compat declared variant | — | A1 | todo |
| A6 | Manifest-declared ambient sources | — | A1 | todo |
| P1 | Structured ApiError | — | M1 | todo |
| P2 | UpgradePlan::diff full binding | — | M3 | todo |
| P3 | Mount CRUD endpoints + Registry lock | — | P1, M3 | todo |
| P4 | Daemon-enforced upgrade consent | — | P2, P3 | todo |
| P5 | Backend identity on the wire | — | P1 | todo |
| P6 | Credential health + reload endpoints | — | A2, P1 | todo |
| P7 | Providers endpoint | — | P1, M3 | todo |
| P8 | Scoped reconcile + busy signal | — | P1 | todo |
| P9 | Control-port bearer token | GATED | P1 | todo |
| C1 | CLI structural fixes | — | P5 | todo |
| C2 | Grammar, --json, exit codes, hints | — | P1, C1 | todo |
| C3 | Setup wizard + express init | — | C2, P3 | todo |
| C4 | Golden-path e2e (K2 instrumentation) | — | C3, T2 | todo |
| G1 | One inheritance function | — | M3 | todo |
| G2 | Atomic writes + precedence helper + single-owner constants | PARALLEL-OK | M3 | todo |
| G3 | Inject-domain ⊆ capability-need validation | — | M3 | todo |
| G4 | Guest-path CI grep + Dockerfile dedup | PARALLEL-OK | PF | todo |
| G5 | dev.ts through the CLI | — | C2 | todo |
| F1 | FUSE async-first dispatch | LINUX | N3 | todo |
| F2 | engine::render toolkit + frontend migration | LINUX | M4, F1 | todo |
| F3 | Shared materialize cap | — | F2 | todo |
| F4 | Pagination projection face into tree | — | M4 | todo |
| F5 | ServingContext + Mounts::Single removal | — | M4 | todo |
| S1 | SDK P0: route export, error context, version evidence, event drops | — | N2 | todo |
| S2 | router/object.rs split + ServeCtx + typed ObjectKind | — | S1 | todo |
| S3 | Macro unification + helpers/pattern cleanup | — | S1 | todo |
| S4 | DirProjection removal + two-stage registration | — | S2 | todo |
| L1 | README/schema synthesis (D10) | — | S1 | todo |
| L2 | omnifs usage skill | — | L1 | todo |
| L3 | Staleness backstop | — | M4 | todo |
| L4 | web provider | — | S1 | todo |
| L5 | GitHub deepening | — | S1 | todo |
| W1 | Worldview v0 | GATED | F5, P3, C3 | todo |
| W2 | Replica snapshot + export | — | M4 | todo |
| H1 | Liveness (subscription callouts) | HORIZON | — | not planned here |
| H2 | MCP bridges, inspector web dashboard, devcontainer/CI packaging | HORIZON | — | not planned here |

`[HORIZON]` steps are re-planned only after T4's numbers are reviewed by Raul.

---

## 2. PF: preflight

**Goal.** A verified-green baseline and a recorded environment.

**Do.**
1. `git -C /path/to/omnifs status --short` must be clean on `main`; `git pull`.
2. Install toolchain if missing: `just providers wasi-sdk`.
3. Run and record: `GATE-FMT`, `GATE-PROVIDERS`, `GATE-HOST`, `GATE-TEST`. All must pass on unmodified `main`. If any fails on clean `main`, stop (status `blocked-failed` on PF) — the baseline is broken and that is Raul's problem, not yours.
4. Record in the PR-less ledger note: OS, whether a Linux builder is available (attempt `uname -s` on any configured remote builder; if none, note it — M2, U2, F1, F2 will block).
5. Read, once, in this order: `docs/future/bluesky-rewrite.md`, `docs/future/bluesky-tree.md`, `docs/future/cli-experience.md`, `AGENTS.md`. You now have design context; this file still governs execution.

No PR. Set PF `done` in the ledger via a docs-only commit on a branch `bluesky/PF-ledger` (PR title: `docs(plan): record preflight baseline`).

---

## 3. Phase T: the truth track

### T1. Latency measurement suite `[PARALLEL-OK]`

**Goal.** Reproducible p50/p95 numbers for warm and cold `ls`/`cat`/`grep -r` under concurrency, against the live runtime. This is the K3 instrument.

**Do.**
1. Create `benchmarks/latency/run.ts` (Bun, like `scripts/dev.ts`). It must:
   - Accept `--target <path>` (a mounted omnifs directory), `--concurrency 1|4|8`, `--iterations N` (default 50), `--out <file>`.
   - Scenarios, each timed with `performance.now()` around a spawned process (`Bun.spawn`), never shell pipelines: `ls <target>`, `ls <target>/<first-subdir>`, `cat <first-file>`, `grep -r <literal-present-string> <target-subdir>`.
   - Warm protocol: run each scenario once untimed (populates caches), then N timed iterations. Cold protocol: only for the first post-daemon-start iteration; record it separately (restart is orchestrated by the caller, documented in the README).
   - Concurrency: launch C copies of the scenario simultaneously with `Promise.all`, record each duration.
   - Output JSON: `{ date, target, git_sha, scenarios: [{ name, concurrency, warm: {p50_ms, p95_ms, n}, cold_first_ms }] }` plus a rendered Markdown table beside it.
2. Create `benchmarks/latency/README.md` documenting: how to start the runtime (`just dev -y`, target `/omnifs` inside the container via `docker exec`; or a host-native mount), the warm/cold protocol, and that thresholds (K3: warm p95 ≤ 50ms at concurrency 8) are recorded, not enforced.
3. Run it once against the dev runtime; commit the first report to `benchmarks/reports/latency-<YYYY-MM-DD>.{json,md}`.

**Gates.** The script exits 0; the JSON parses (`bun -e 'JSON.parse(await Bun.file(process.argv[1]).text())' <file>; echo exit=$?` → 0); the report is committed.

**PR.** `feat(bench): latency measurement suite with first recorded report`

### T2. Dogfood telemetry counters `[PARALLEL-OK]`

**Goal.** Local-only denominators for kill criteria K2 (mount sessions surviving without manual recovery) and K5 (weekly-active use). Privacy rule, stated in code comments and docs: written under the workspace, mode 0600, never transmitted anywhere, no network code may touch it.

**Do.**
1. In the daemon (today `crates/omnifs-daemon/src/`; post-M4 unchanged): on start, frontend-serving, frontend-unmount, and shutdown, append one JSON line to `<config_dir>/telemetry/daemon.jsonl`: `{"ts":"<rfc3339>","event":"daemon_start"|"frontend_serving"|"frontend_stopped"|"daemon_stop","backend":"docker"|"native","mounts":<n>}`. Create the directory 0700, file 0600. Write failures are logged at debug and never propagate.
2. In the CLI (`crates/omnifs-cli/src/main.rs` exit path): append `{"ts","cmd":"<top-level subcommand>","exit":<code>}` to `<config_dir>/telemetry/cli.jsonl`, same permissions and failure rule.
3. Config off-switch: `[telemetry] enabled = false` in the strict CLI `Config` (`crates/omnifs-cli/src/config.rs`), read by both writers (the daemon receives it via an existing config channel or an env var `OMNIFS_TELEMETRY=0`; pick the env var if no config channel exists today, and say so in the PR).
4. `benchmarks/latency/../dogfood-report.ts` → put at `scripts/bench/dogfood-report.ts`: reads both files, prints sessions count, median session duration, recoveries (a `daemon_start` within 5 minutes of a `frontend_stopped` without `daemon_stop` counts as a recovery), and weekly-active days.
5. Unit tests for the JSONL append (permission bits, malformed-line tolerance in the reporter).

**Gates.** `GATE-TEST`; manual check: run `just dev -y`, `docker exec omnifs omnifs status`, stop it, then `bun scripts/bench/dogfood-report.ts --home ~/.omnifs-dev` prints ≥1 session; file mode is 0600 (`stat -f %Lp` on macOS / `stat -c %a` on Linux → `600`).

**PR.** `feat(telemetry): local-only dogfood counters for daemon sessions and CLI usage`

### T3. Benchmark harness, deterministic condition `[PARALLEL-OK]`

**Goal.** The K1 instrument: same tasks, same model, two conditions (mounted tree vs an MCP baseline), measuring success, tokens, wall clock, and operation count. Deterministic v1 runs against fixture data so it is repeatable and free of upstream flakiness.

**Do.**
1. Layout: `benchmarks/agentbench/{tasks/,mcp-baseline/,runner.ts,grade.ts,README.md}`.
2. Task file schema (`tasks/*.yaml`), one task per file:
   ```yaml
   id: corr-001
   family: correlation | navigation | aggregation | reconstruction
   prompt: "<the user prompt given to the agent verbatim>"
   success:
     type: contains | regex | judge
     value: "<string, regex, or judging rubric sentence>"
   max_turns: 30
   ```
3. Fixture corpus: a directory `benchmarks/agentbench/fixture-data/` of ~200 small files across a synthetic "github-like" layout (`repos/<r>/issues/<n>/{body.md,comments/}`) and a "linear-like" layout (`issues/<KEY>/…`), with planted facts that tasks ask about (cross-references between the two trees for the correlation family; numeric counts for the aggregation family, where SQL/tools SHOULD win — keep the benchmark honest).
4. Condition A (mount): serve the fixture corpus through omnifs. Use the existing test provider if it can serve a preopened directory (check `providers/test/src/lib.rs` for a treeref/preopen route; if absent, add a `fixture` route to providers/test that treerefs a preopened host directory — preopens are an existing capability kind). The agent runs with `cwd` set to the mounted fixture root.
5. Condition B (MCP baseline): `mcp-baseline/server.ts`, a Bun stdio MCP server exposing exactly two tools, `list_dir(path)` and `read_file(path)`, backed by the same `fixture-data/` directory. This is the structurally-honest baseline for v1 (same data, tool-call interaction model); the live run (T4) upgrades to real vendor MCP servers.
6. Runner: for each task × condition, invoke the Claude Code CLI headlessly. Probe the exact flags first with `claude --help` (they drift); the intended invocation shape is: print mode with JSON output (`claude -p "<prompt>" --output-format json`), tool restriction to file tools + bash for condition A, and `--mcp-config` pointing at the baseline server with only MCP tools allowed for condition B, `--max-turns` from the task. Parse the JSON result for token usage, turn count, and duration; grade with `grade.ts` (`contains`/`regex` locally; `judge` via one additional model call with the rubric — judge calls are part of T4's cost approval, so in T3 restrict tasks to `contains`/`regex`).
7. Output: `benchmarks/reports/agentbench-<date>.json` with per-task rows `{id, family, condition, success, tokens_total, turns, wall_ms}` and a Markdown summary table with per-family aggregates and the two headline numbers K1 cares about: total-token ratio and success delta.
8. README: exact reproduction steps, including model version pinning (record the model id in the report).
9. Smoke-run the harness with `--tasks corr-001 --dry-run` (dry-run executes everything except the model call, substituting a canned transcript) to prove the plumbing without cost.

**Gates.** Dry-run exits 0 and emits a well-formed report; `GATE-TEST` (any Rust touched); fixture provider change passes `GATE-PROVIDERS` and the conformance path (`cargo nextest run -p omnifs-itest; echo exit=$?` → 0).

**PR.** `feat(bench): agent benchmark harness with deterministic fixture condition`

### T4. Live benchmark + M0 report `[COST] [GATED]`

**Goal.** The real numbers. Requires Raul's approval before any model-invoking run.

**Do (after approval).**
1. Add live task variants against the real github and linear providers (a dedicated test org/workspace whose contents you seed and record in the README), and a condition B using the official GitHub MCP server (and Linear's, if available) configured against the same org.
2. Run the full suite 3× per condition; report medians. Include the aggregation family in the headline even if it loses; the report states where files win and where they don't.
3. Run T1 against both frontends on the same machine; include latency in the report.
4. Write `benchmarks/reports/M0-report.md`: the K1 verdict fields (total-token reduction %, success delta pp, per family), the K3 verdict (warm p95 at concurrency 8), and no editorializing.
5. Stop. Present the report. K1/K3 evaluation and every `[HORIZON]` planning decision is Raul's.

**PR.** `docs(bench): M0 benchmark and latency report`

---

## 4. Phase N: the regression net

### N1. Land the phase-a deletions branch

**Do.** `git log raulk/phase-a-deletions --oneline` — verify it contains exactly: the `Workspace<Role>` phantom removal (omnifs-home), dead view-cache method removals (`Cache::{invalidate, invalidate_entries_if, is_fresh}`), and the empty inspector allowlist removal. Rebase onto current `main`, resolve mechanically, run `GATE-FMT`, `GATE-TEST`, `GATE-HOST`. Grep-gates: `rg -c "Workspace<" crates/omnifs-home/src` → no matches; `rg -c "ALLOWLISTED_QUERY_KEYS" crates/` → no matches.

**PR.** `refactor: remove Workspace<Role> phantom, dead view-cache methods, empty inspector allowlist`

### N2. Characterization test net

**Goal.** Freeze current behavior before anything moves. Every test asserts what the code DOES today; if a test reveals a bug, record it in the PR body and characterize the buggy behavior anyway (fixes come later, at their planned step).

**Do.** Add these tests (names are binding):
1. `crates/omnifs-itest/tests/pagination_exhaustive.rs`: through the kernel-free `Tree` harness against the test provider, list a paged directory; assert `@next` appears; read `@next` repeatedly; assert the final listing contains every fixture entry exactly once and `@next` disappears; assert `@all` materializes the same complete set.
2. `crates/omnifs-host/tests/effects_semantics.rs`: drive a provider return with canonical-store + fs + invalidation effects in one batch (via the existing `__test_support` harness in `crates/omnifs-host/src/runtime.rs`); assert application order (canonical batch first, fs second, dirents merge third, invalidations last) by observing cache state between crafted ops; assert an `Invalidation::Object` deletes the durable object (subsequent cold read misses) rather than only fencing.
3. `crates/omnifs-host/tests/auth_refresh_characterize.rs`: using `FakeAuthServer` (`crates/omnifs-auth/src/client/test_support.rs`) and a `MemoryStore`, build an `AuthManager` with an expiring OAuth credential; assert (a) a request inside the 60s window triggers a synchronous refresh, (b) a 401 response triggers refresh-and-retry-once, (c) an `invalid_grant` refresh DELETES the stored credential (today's behavior; A3 changes it and must update this test intentionally).
4. `crates/omnifs-itest/tests/live_growth.rs` (or extend the existing live-follow coverage; check `rg -l "live_follow|tail" crates/omnifs-itest/tests/` first): a growing file's learned size is promoted and a follow-mode read observes appended bytes.
5. OAuth loopback characterization in `crates/omnifs-auth` tests already covers bind/CSRF/port (verify by `cargo nextest run -p omnifs-auth; echo exit=$?` → 0 and reading the test list); add any missing case (non-GET rejected, state mismatch rejected) rather than duplicating.
6. `crates/omnifs-host/tests/reconcile_machine.rs`: verify add/update/remove/failure-isolation transitions if not already covered by `registry.rs` in-crate tests (`rg -n "reconcile_keeps_running_mount" crates/omnifs-host/src/registry.rs` — extend, don't duplicate).
7. Inspector redaction: assert Authorization headers and query strings are stripped from emitted records (`crates/omnifs-host/tests/` or in-crate where the sink lives).

**Gates.** `GATE-TEST`, `GATE-HOST`, `GATE-PROVIDERS` (test-provider edits). Every new test passes and is listed in the PR body.

**PR.** `test: characterization net for pagination, effects, auth refresh, live growth, reconcile, redaction`

### N3. Frontend concurrency net

**Do.**
1. Add to `providers/test` a route `slow/{ms}` (integer capture, cap 10000): a file whose read performs an HTTP callout to the itest fixture server, which sleeps `ms` before responding (add the sleep endpoint to the itest fixture server; find it via `rg -l "fixture" crates/omnifs-itest/src/`).
2. New live NFS test (macOS-capable, respect the existing cross-process serialization lock; find it via `rg -n "lock" crates/omnifs-nfs/tests/nfs_real_provider_smoke.rs`): start a read of `slow/5000`, then within 500ms `stat` and `cat` a fast path on the same mount; assert the fast operations complete in under 2000ms while the slow read is still pending. This test will FAIL against FUSE today (head-of-line blocking) — it is NFS-only until F1; mark the FUSE variant `#[ignore = "enabled by F1"]` with that exact string.

**Gates.** `GATE-PROVIDERS`, `GATE-TEST`, the new NFS test passes locally (`cargo nextest run -p omnifs-nfs --run-ignored default; echo exit=$?` → 0 — do not interrupt live NFS tests; an interrupted run orphans mounts).

**PR.** `test(frontends): concurrency net proving slow provider ops must not block the mount`

---

## 5. Phase M: consolidation merges

Mechanics common to M1-M5: use `git mv` so history follows; prefer Serena/ast-grep for import rewrites (`ast-grep --lang rust -p 'use omnifs_inspector::$$$'` etc.); after each merge delete the old crate from `[workspace] members` in the root `Cargo.toml`; update `AGENTS.md` "Orientation" and the `docs/contracts/*` "Code" listings for the moved paths in the same PR; keep every intermediate commit compiling.

### M1. omnifs-api absorbs omnifs-inspector

**Do.**
1. `git mv crates/omnifs-inspector/src crates/omnifs-api/src/events` (adjust: move the module files under `crates/omnifs-api/src/events/`, add `pub mod events;` to `crates/omnifs-api/src/lib.rs`).
2. Merge Cargo dependencies of omnifs-inspector into omnifs-api; delete `crates/omnifs-inspector`.
3. Rewrite all `omnifs_inspector::` imports to `omnifs_api::events::` (consumers today: `crates/omnifs-host/src/inspector.rs`, `crates/omnifs-cli/src/inspector/*`, `crates/omnifs-tree`, `crates/omnifs-fuse`; find them all with `rg -l "omnifs_inspector"`).
4. Convert the free functions `parse_record`, `parse_record_line`, `serialize_record` (`events/wire.rs`) into `InspectorRecord::{parse, parse_line, to_json}`; update call sites.

**Gates.** `cargo tree -i omnifs-api | rg -c "omnifs-(host|cli|daemon)"` ≥ 3 (consumers intact); `rg -c "omnifs_inspector" crates/ providers/` → no matches; `GATE-TEST`, `GATE-HOST`, `GATE-OPENAPI` (schema unchanged → regeneration is a no-op; if it changed, stop: this step must be wire-neutral).

**PR.** `refactor(api): absorb omnifs-inspector as the events half of the control-plane wire contract`

### M2. Extract omnifs-mtab `[LINUX]`

**Do.**
1. New crate `crates/omnifs-mtab` (deps: `omnifs-core`, serde, thiserror). Three modules:
   - `proc_mounts.rs`: move the parser + octal decoder from `crates/omnifs-daemon/src/proc_mounts.rs` (13-53) — it is byte-identical to `crates/omnifs-nfs/src/mount.rs:427-465`; keep ONE copy, unify the two `MountInfo`-like structs into one `MountEntry { device, mount_point, fs_type }`, delete both originals. `cfg(target_os = "linux")` on this module.
   - `state.rs`: move `NfsMountState`, its `VERSION` const, `read_mount_states`/`read_mount_state_file`/`write_state` from `crates/omnifs-nfs/src/mount.rs` (as methods: `NfsMountState::read_all(dir)`, `NfsMountState::read_file(path)`, `StateFile::write(...)`); delete the duplicated `STATE_VERSION` in `crates/omnifs-cli/src/host_teardown.rs:25` and point the CLI at the one type.
   - `unmount.rs`: one `UnmountCommand` with `graceful(platform, mount_point)` and `forced(platform, mount_point)` constructors, built from the union of `omnifs-nfs/src/mount.rs:162-225` and `omnifs-cli/src/host_teardown.rs:268-357` (macOS: `diskutil unmount [force]`; Linux: `fusermount -u [-z]`, `umount -f` fallback). The CLI and nfs both call it.
2. Rewire `omnifs-daemon`, `omnifs-nfs`, `omnifs-cli` onto the crate; delete the originals.

**Gates.** `rg -c "decode_proc_mount_field|decode_mount_field" crates/omnifs-daemon crates/omnifs-nfs` → no matches; `GATE-TEST`, `GATE-HOST`; Linux build of the daemon (`cargo build -p omnifs-daemon --target-dir /tmp/lin` ON A LINUX HOST; if unavailable → `blocked-linux`); macOS NFS live smoke (`cargo nextest run -p omnifs-nfs; echo exit=$?` → 0).

**PR.** `refactor(mtab): single owner for proc-mounts parsing, NFS mount state, and unmount mechanics`

### M3. omnifs-workspace merge `[GATED(utoipa)]`

**Goal.** One crate owning every byte under OMNIFS_HOME. Merges omnifs-mount + omnifs-provider + omnifs-creds + omnifs-home, and receives core's non-guest types.

**Do.**
1. Create `crates/omnifs-workspace` with the module layout from `docs/future/bluesky-tree.md` §omnifs-workspace, concretely:
   - `layout.rs` ← `crates/omnifs-home/src/lib.rs` (whole crate).
   - `ids.rs` ← `crates/omnifs-core/src/provider.rs` (`ProviderName`, `ProviderId`, `ProviderVersion`, `ProviderMeta`, `ProviderRef` + errors). Put every `utoipa::ToSchema` derive behind a crate feature `utoipa` (default OFF), and enable the feature only from crates that need schema generation. **GATED**: state in the PR body that this changes the gated OpenAPI/ToSchema surface and wait for sign-off before merge.
   - `authn/ids.rs` ← `crates/omnifs-core/src/auth.rs` (`SchemeId`, `AccountId`, `CredentialId`, `AuthKind`). Add `CredentialId::for_mount(provider: &ProviderName, auth: &crate::mounts::Auth) -> CredentialId` implementing exactly the derivation both call sites use today (scheme from `auth.scheme()` or the manifest default resolved by the caller; account from `auth.account()` or `AccountId::default_account()`); do NOT yet rewire host/CLI (that is A1) — just provide the function with unit tests proving it matches both existing derivations (`crates/omnifs-cli/src/credential_target.rs:95-113` and `crates/omnifs-host/src/auth.rs:294-299,343-348`).
   - `authn/scheme.rs` ← `crates/omnifs-provider/src/auth_wire.rs`; add `#[serde(deny_unknown_fields)]` to every struct in the module (today only `TokenValidation` has it). `authn/resolve.rs` ← `auth_resolve.rs`.
   - `provider/` ← the rest of `crates/omnifs-provider/src/*` unchanged. `mounts/` ← `crates/omnifs-mount/src/*` unchanged, plus `mount::Name` ← `crates/omnifs-core/src/mount.rs`. `creds/` ← `crates/omnifs-creds/src/*` unchanged. `io.rs`: new, wrapping the `atomic_write_file` crate (already a dep of creds) as `pub fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()>`; do not yet migrate writers (G2 does).
2. Rewire every consumer (`rg -l "omnifs_mount|omnifs_provider|omnifs_creds|omnifs_home" crates/ providers/ | sort`) to `omnifs_workspace::...`. Providers and `omnifs-sdk` must NOT gain the dependency: verify their Cargo.tomls untouched.
3. Delete the four old crates; update `omnifs-embed-metadata` (it deps omnifs-provider) and `scripts` references.

**Gates.** `cargo tree -p omnifs-sdk -e normal | rg "omnifs-" | sort -u` → exactly `omnifs-core`, `omnifs-sdk-macros`, `omnifs-wit` (plus self); `cargo tree -p omnifs-auth -e normal | rg -c "omnifs-(engine|host|tree)"` → 0; `rg -c "deny_unknown_fields" crates/omnifs-workspace/src/authn/scheme.rs` ≥ 8; `GATE-TEST`, `GATE-HOST`, `GATE-PROVIDERS`, `GATE-SCHEMA` (manifest schema output must be byte-identical; if not, stop and inspect), `GATE-LIVE`.

**PR.** `refactor(workspace)!: one crate owns every OMNIFS_HOME format (mount+provider+creds+home merge)`

### M4. omnifs-engine merge + visibility

**Goal.** One trusted-runtime crate (host + tree + cache + core::view), public surface curated and CI-pinned.

**Do.**
1. Create `crates/omnifs-engine`; move, preserving module structure per `docs/future/bluesky-tree.md` §omnifs-engine: `crates/omnifs-host/src/*` → `engine/src/{runtime/,ops/,callouts/,...}` (mechanical mapping: `runtime.rs`→`runtime/mod.rs`, `instance.rs`→`runtime/instance.rs`, `registry.rs`→`runtime/registry.rs`, `wasm/`→`runtime/wasm.rs`, `wasi.rs`→`runtime/wasi.rs`, `op.rs|op_lifecycle.rs|op_validate.rs|namespace.rs`→`ops/`, `callouts.rs|http.rs|git.rs|cloner.rs|blob.rs|archive.rs`→`callouts/`, `auth.rs`→`auth_inject.rs`, `materialize.rs|invalidation.rs`→`effects/`, `blob_cache.rs`→`cache/blob.rs`, `wit_protocol.rs`→`callouts/wit_convert.rs`, `path_key.rs`→`render/identity.rs` stub, `inspector.rs`→`inspect.rs`, rest 1:1); `crates/omnifs-cache/src/*` → `engine/src/cache/`; `crates/omnifs-tree/src/*` → `engine/src/tree/`; `crates/omnifs-core/src/view.rs` → `engine/src/view.rs`.
2. Split the test harness out of the old `runtime.rs`: `TestOp`, `TestOpState`, `PendingTestCallout`, `__test_support` → `engine/src/test_support.rs`, same cfg-gating.
3. Visibility pass: `engine/src/lib.rs` re-exports ONLY: `Engine`/`MountRuntimes` (registry), `Tree` + tree public types (`Node`, `NodeBody`, `Entry`, `Listing`, `ReadResult`, `RangedHandle`, `InvalidationReport`, `TreeError`, `TreeErrorKind`, `RequestCtx`), `render::*`, `view::*`, `test_support` (cfg-gated), the events stream handle, and error types. Everything else `pub(crate)`. Namespace outcome materialization (making all five ops return domain types so `tree/` stops importing `omnifs_wit`) is REQUIRED here, not later: extend the `LookupOutcome` pattern in `ops/namespace.rs` to `list_children`/`read_file`/`open_file`/`read_chunk` (`ListOutcome`, `ReadOutcome`, `OpenOutcome`, `ChunkOutcome`, each mirroring the WIT variants the tree currently matches in `tree/list.rs`, `tree/read.rs`, `tree/handle.rs`, `tree/error.rs`); move the conversion code from those tree files into `ops/namespace.rs`; make `callouts/wit_convert.rs` `pub(crate)`; while there, collapse the five duplicated dispatch-match blocks in `namespace.rs` into one generic `run_op_expect`.
4. Create `scripts/ci/check-engine-surface.sh`: extracts `^pub ` lines from `crates/omnifs-engine/src/lib.rs`, diffs against checked-in `scripts/ci/engine-surface.txt`, exits non-zero on any difference. Wire it into the CI lane that runs host gates (find the workflow with `rg -l "host clippy\|host test" .github/workflows/`).
5. Rewire consumers (`omnifs-fuse`, `omnifs-nfs`, `omnifs-daemon`, `omnifs-itest`); move `omnifs-nfs`'s `omnifs-cache` dependency to dev-dependencies (it becomes an `omnifs-engine` dev-dep for its test's cache poking, or the test is rewritten against `test_support`). Delete the three old crates. Delete `crates/omnifs-host/src/manifest.rs` (`Artifact`) by inlining its two lines into `Runtime::build` (`omnifs_workspace::provider::ProviderWasm::from_bytes(fs::read(path)?)?.metadata()`).

**Gates.** `bash scripts/ci/check-engine-surface.sh; echo exit=$?` → 0; `cargo tree -p omnifs-engine -e normal --invert omnifs-wit 2>/dev/null | rg -c "omnifs-fuse|omnifs-nfs"` → 0 AND `rg -c "omnifs_wit" crates/omnifs-fuse/src crates/omnifs-nfs/src crates/omnifs-engine/src/tree` → no matches; `rg -c "omnifs_host|omnifs_tree|omnifs_cache" crates/ providers/` → no matches; `GATE-TEST`, `GATE-HOST`, `GATE-PROVIDERS`, `GATE-LIVE`; the N2/N3 nets green.

**PR.** `refactor(engine)!: one trusted-runtime crate (host+tree+cache+view) with a compiler-enforced frontend surface`

### M5. omnifs-auth retarget

**Do.** Point `omnifs-auth`'s Cargo deps at `omnifs-workspace` (for `CredentialEntry`, store, `OauthScheme`, mount `OAuth` config) and `omnifs-core`; delete its `omnifs-mount`/`omnifs-provider` deps (already gone as crates — this step is the import rewrite plus verifying no engine edge appears). Split `client.rs` (1,110 lines) into `client.rs` (the `OAuthClient` facade), `flows/{loopback,device,manual,implicit}.rs`, `callback.rs` (`LoopbackCallback::parse`, `ClientSideTokenCallback::parse` as associated fns), keeping behavior identical; collapse the three duplicated `From<oauth2::RequestTokenError<...>>` impls into one generic helper; make `token_endpoint_secret` a zero-argument `&self` method on `OAuthRequest` (both call sites pass only its own fields); rename `oauth_request_from_config` → `OAuthRequest::from_mount_config`.

**Gates.** `cargo tree -p omnifs-auth -e normal | rg "omnifs-" | sort -u` → exactly `omnifs-core`, `omnifs-workspace` (plus self); `cargo nextest run -p omnifs-auth; echo exit=$?` → 0; `GATE-TEST`.

**PR.** `refactor(auth): retarget onto workspace and restructure the flow client`

---

## 6. Phase U: naming and contract unification

### U1. Naming batch 1 (macOS-safe)

**Do.** In separate commits within one PR, using Serena rename where possible:
1. `omnifs_engine::runtime::ProviderRegistry` → `MountRuntimes` (kills the collision with `workspace::mounts::Registry`).
2. `omnifs_engine::effects::Materializer` → `EffectApplier`; split its 193-line `apply` into per-effect-kind private methods (`apply_canonical_batch`, `apply_fs_effects`, `merge_dirents`, `apply_invalidations`) with identical behavior (N2's `effects_semantics.rs` must stay green unmodified).
3. `omnifs_engine::cache::view::Freshness` → `Expiry` (kills the collision with `view::Freshness`).
4. `mount_fingerprint` switches from `DefaultHasher` to `blake3` over the serialized materialized spec (blake3 is already a workspace dep; verify with `rg "blake3" Cargo.toml crates/*/Cargo.toml`).
5. Error-name qualification: `workspace::provider::StoreError` → `ProviderStoreError` OR `creds::StoreError` → `CredStoreError` (pick: rename the creds one), `workspace` mounts `Error` → `SpecError`, engine runtime `Error` → `EngineError`, `omnifs-auth`'s and engine's `AuthError` collision → engine's becomes `InjectError`. Update `From` impls and call sites.
6. `derive` → `computed` on the SDK object face (macro keyword + SDK types + all provider call sites: `rg -l "\.derive\(|derive\!" providers/ crates/omnifs-sdk`), and `Canonical` role-naming: verify only the SDK type is named `Canonical` and every other appearance is a role-named projection (`rg -n "struct Canonical|enum Canonical" crates/` → exactly one hit in the SDK; if more, rename the others as `<Role>Canonical` per the rewrite doc).
7. `InspectorFuseScope` → `InspectorRequestScope`; fix `tools/mod.rs`'s stale "Wasm tools" doc comment.

**Gates.** `rg -c "ProviderRegistry|Materializer\b|InspectorFuseScope" crates/` → no matches; `GATE-TEST`, `GATE-HOST`, `GATE-PROVIDERS` (face keyword change rebuilds providers; tracers github/linear/docker must pass conformance: `cargo nextest run -p omnifs-itest; echo exit=$?` → 0), `GATE-LIVE`.

**PR.** `refactor: one concept one name — registry/effects/expiry/error renames, computed face, blake3 fingerprint`

### U2. Naming batch 2: Backing → Subtree `[LINUX]`

**Do.** Rename the `Backing` occurrences up to `Subtree` (the WIT and SDK already say `Subtree`): the tree read-result variant and the fuse inode body variant (`rg -n "Backing" crates/omnifs-engine/src crates/omnifs-fuse/src crates/omnifs-nfs/src` — expect ~17 hits in nfs/adapter.rs plus the fuse cluster). No contract change (verify `git diff crates/omnifs-wit` is empty).

**Gates.** `rg -c "Backing" crates/` → no matches outside comments; Linux: `cargo build -p omnifs-fuse` + `GATE-HOST` on the Linux host; macOS: `cargo nextest run -p omnifs-nfs; echo exit=$?` → 0; `GATE-LIVE`.

**PR.** `refactor(tree): Backing renamed up to Subtree across read path and frontends`

### U3. Strict top-level Spec parsing `[GATED]`

**Do.** Add `#[serde(deny_unknown_fields)]` to `workspace::mounts::Spec` and to the provider-store `Index`/`IndexEntry`. Add a test: a spec JSON with a typo'd top-level key fails to parse with an error naming the key. Check every fixture (`providers/*/dev/mount.json`, itest fixtures) still parses (`rg -l "mount.json" providers/ | xargs -I{} echo {}` then run `GATE-TEST` + `GATE-LIVE`).

**Gate + stop.** Open the PR with body line: "Gated decision: strict mount-spec parsing (AGENTS.md footgun says raw specs are permissive today; this closes it). Awaiting sign-off." Set `blocked-gated`.

**PR.** `feat(workspace)!: strict top-level mount Spec parsing`

### U4. Need → AccessNeed/ResourceLimit split (the contract slice)

**Do.** One PR moving together, per `docs/future/capability-limits-split.shapediff.md` (read it first; it is the authoritative shape diff):
1. WIT: split the `capability-need` variant (`crates/omnifs-wit/wit/provider.wit`, the variant near line 421) into `access-need` and `resource-limit` arms.
2. `omnifs-caps`: `Need` → `AccessNeed` + new `ResourceLimit`; `Grants::satisfies` covers access needs only; delete the unreachable blob-byte arms in the SDK macro.
3. SDK macro emission, `workspace::provider::manifest` wire types, `workspace::mounts` grant handling, engine `CapabilityChecker` — all updated in the same PR.
4. Every provider under `providers/` rebuilt and updated in the same PR. `cargo clean -p omnifs-wit` before builds.

**Gates.** `GATE-PROVIDERS`, `GATE-SCHEMA` (schema change is intentional; commit the regenerated schema), `GATE-HOST`, `GATE-TEST`, conformance (`cargo nextest run -p omnifs-itest`), `GATE-LIVE`, and the host integration path that seals providers: `cargo nextest run -E 'test(all_providers_initialize_and_seal)'; echo exit=$?` → 0.

**PR.** `feat(caps)!: split capability needs into AccessNeed and ResourceLimit across WIT, SDK, manifest, and providers`

---

## 7. Phase A: the auth subsystem

### A1. CredentialService core

**Do.**
1. In `crates/omnifs-auth/src/service.rs`, implement:
   ```rust
   pub struct CredentialService { /* store: Arc<dyn CredentialStore>, oauth: OAuthClient,
       states: DashMap<CredentialId, CredentialState> */ }
   pub enum CredentialHealth { Ready, ExpiringSoon, Expired,
       RefreshFailed { attempts: u32 }, NeedsConsent, Missing, StaticUnvalidated }
   pub struct CredentialStatus { pub id: CredentialId, pub health: CredentialHealth,
       pub expires_at: Option<OffsetDateTime>, pub scopes: Vec<String> } // NEVER token material
   impl CredentialService {
       pub fn new(store: Arc<dyn CredentialStore>, oauth: OAuthClient) -> Self;
       pub async fn authorization(&self, id: &CredentialId) -> Result<HeaderMaterial, AuthUnavailable>;
       pub fn health(&self) -> Vec<CredentialStatus>;
       pub fn store_entry(&self, id: &CredentialId, entry: CredentialEntry) -> Result<(), CredStoreError>;
       pub async fn reload(&self, id: &CredentialId);
   }
   ```
   `HeaderMaterial` = the resolved header name + secret value pair(s) the injector composes; `AuthUnavailable` = typed enum { Missing, NeedsConsent, Expired, RefreshFailed }. `authorization` re-reads the store on miss, refreshes synchronously when inside the window (single-flight per id — reuse the `async_singleflight` pattern from `omnifs-host/src/auth.rs`), and fails closed.
2. ONE expiry function: move `CredentialEntry::is_expired_at` usage behind `service::is_fresh(entry, now) -> bool` with a single `pub const REFRESH_WINDOW: Duration = Duration::from_secs(60)`; delete `OAUTH_REFRESH_WINDOW`, `oauth_entry_is_valid`, `oauth_entry_is_fresh` from `engine/src/auth_inject.rs`, and the mint-time 1-minute skew in `credential_entry_from_token` (`flows/` after M5) so expiry is computed at check time, not baked at mint time. Update N2's characterization test intentionally (PR body must say which assertions changed and why).
3. ONE key derivation: rewire `engine/src/auth_inject.rs` and `crates/omnifs-cli/src/credential_target.rs` onto `CredentialId::for_mount` (from M3); delete engine's `DEFAULT_ACCOUNT` const and the CLI's `internal_from_parts`.
4. Engine wiring: `Runtime::build` takes `Arc<CredentialService>` (threaded from the daemon context and from the CLI's daemon-less paths); `auth_inject` keeps domain matching + header composition and calls `service.authorization`.
5. CLI login flows call `service.store_entry` instead of writing `FileStore` directly (`rg -n "store.put" crates/omnifs-cli/src/auth`).

**Gates.** `rg -c "OAUTH_REFRESH_WINDOW|oauth_entry_is_fresh|DEFAULT_ACCOUNT" crates/` → no matches; `cargo nextest run -p omnifs-auth -p omnifs-engine; echo exit=$?` → 0; `GATE-TEST`, `GATE-HOST`, `GATE-LIVE` with an OAuth mount (github via `just dev` profile).

**PR.** `feat(auth): CredentialService owns store access, the single expiry function, and the single key derivation`

### A2. Proactive refresh loop + health + mount-start validation

**Do.**
1. `CredentialService::spawn_refresh_loop(self: &Arc<Self>) -> JoinHandle<()>`: a tokio task that computes the earliest `(expires_at - REFRESH_WINDOW)` across refreshable OAuth credentials, sleeps until then (+ jitter up to 10% of the interval, from a seeded small PRNG), refreshes with per-id single-flight, updates health, repeats. Wake immediately on `reload`/`store_entry` (a `tokio::sync::Notify`).
2. Daemon spawns the loop in `app.rs` after building the service; aborts it on shutdown.
3. Mount-start validation: `Runtime::build` queries `service` health for the mount's credential; `Missing`/`Expired`/`NeedsConsent` logs a warn and records the mount as degraded (surface: the existing failed/health plumbing — the mount still loads, reads that need auth fail closed per A1).
4. Test (extends N2's file): with `FakeAuthServer` and a credential expiring in 2×REFRESH_WINDOW, start the loop, advance (use tokio `start_paused` time), assert the credential was refreshed WITHOUT any request traffic, and health is `Ready`.

**Gates.** the new proactive test passes under `cargo nextest run -p omnifs-auth`; `GATE-TEST`, `GATE-HOST`.

**PR.** `feat(auth): proactive background refresh, credential health table, mount-start validation`

### A3. Rejection backstop + NeedsConsent

**Do.** Add `CredentialService::report_rejected(&self, id, evidence: RejectionEvidence) -> RefreshOutcome` (evidence: `{ status: u16, www_authenticate: Option<String> }`); move the 401 / 403+`invalid_token` classification out of engine (`should_refresh_for_response`, `bearer_invalid_token`) into the service; parse the `WWW-Authenticate` challenge properly (split on commas, match `error="invalid_token"`) instead of substring search; `invalid_grant` on refresh now transitions to `NeedsConsent` WITHOUT deleting the stored entry (update N2's characterization assertion (c), stating the behavior change in the PR body). `HttpStack::send` keeps retry-once but delegates the decision.

**Gates.** `rg -c "invalid_grant" crates/omnifs-engine/src` → no matches; auth tests green; `GATE-TEST`, `GATE-HOST`.

**PR.** `feat(auth): upstream-rejection backstop with NeedsConsent; secrets survive failed refresh`

### A4. Revocation wiring

**Do.** `CredentialService::revoke_and_delete(&self, id) -> RevokeOutcome` calling `OAuthClient::revoke_access_token` (currently zero callers) then `store.delete`; wire `omnifs mounts rm` (`crates/omnifs-cli/src/commands/mounts.rs`, the `delete_credentials` path) and `reset` to it; `--keep-credentials` skips both. Print the outcome (`revoked upstream` / `provider does not support revocation` / `local only`).

**Gates.** `rg -n "revoke_access_token" crates/ | rg -cv "omnifs-auth"` ≥ 1 (a real caller exists); `GATE-TEST`.

**PR.** `feat(auth): revoke upstream on mount removal and reset`

### A5. device_poll_compat declared variant

**Do.** Add `device_poll_compat: DevicePollCompat` (`Rfc8628` default | `ErrorInOkBody`) to the device-flow config in `workspace::authn::scheme` and the authoring DSL; the shim `rewrite_pending_to_error_status` (auth `flows/device.rs`) applies only when the scheme declares `ErrorInOkBody`; set it in `providers/github`'s scheme declaration; delete the vendor-named justification comment. Providers rebuilt in the same PR.

**Gates.** `rg -ci "github" crates/omnifs-auth/src` → 0; `GATE-PROVIDERS`, `GATE-SCHEMA` (intentional, commit it), device-flow tests green for both variants, `GATE-LIVE` github device login (manual: run `omnifs init github` in the dev container and complete the flow — if no interactive session is possible, state so in the PR and mark the manual check pending for Raul).

**PR.** `feat(auth): device-flow compatibility becomes a provider-declared protocol variant`

### A6. Manifest-declared ambient sources

**Do.** Add `ambient_sources: Vec<AmbientSource>` to `StaticTokenScheme` (authoring + wire), where `AmbientSource` = `{ kind: EnvVar { name } | Command { argv: Vec<String> } , note: String }`; migrate the hardcoded probes from `crates/omnifs-cli/src/commands/init/detect.rs:20-49` into the github (env `GITHUB_TOKEN`, command `gh auth token`) and linear (env `LINEAR_API_KEY`) provider declarations; the CLI's detect becomes a generic interpreter over the manifest (env read; command run with a 3s timeout, never a shell, argv exec only). Delete the `match provider_name` block.

**Gates.** `rg -c "\"github\"|\"linear\"" crates/omnifs-cli/src/commands/init/detect.rs` → no matches; `GATE-PROVIDERS`, `GATE-SCHEMA` (intentional), `GATE-TEST`.

**PR.** `feat(auth): ambient credential sources declared by provider manifests`

---

## 8. Phase P: API v2

`API_MAJOR` bumps to 2 exactly once, in P1. Every P-step ends with `GATE-OPENAPI` (regenerate + commit) and the daemon parity test (`cargo nextest run -p omnifs-daemon; echo exit=$?` → 0). `docs/contracts/50-control-plane.md` is updated in P3.

### P1. Structured ApiError

**Do.**
1. In `omnifs-api`: `pub struct ApiError { pub code: ErrorCode, pub message: String, pub detail: Option<serde_json::Value> }`; `pub enum ErrorCode { AuthRequired, ConsentRequired, MountNotFound, SpecInvalid, ProviderMissing, ReconcileBusy, DaemonShuttingDown, Internal }` (snake_case wire). Bump `API_MAJOR` to 2, reset `API_MINOR` to 0.
2. Daemon: every non-2xx response body is `ApiError` (today: one ad hoc 404 text in `mount_inspect`); `MountFailure` gains `pub kind: ErrorCode`.
3. CLI `DaemonClient`: decode `ApiError` on non-2xx; map to `HintedError` via a single hint table `fn hint_for(code: ErrorCode) -> Option<&'static str>` (e.g. `AuthRequired` → "Try: omnifs mounts reauth <name>"). Major-mismatch handling stays.

**Gates.** `GATE-OPENAPI`, parity test, `GATE-TEST`; a unit test asserts every `ErrorCode` variant has a row in the hint table (`match` exhaustiveness, no wildcard arm).

**PR.** `feat(api)!: API v2 with structured errors and a CLI hint table`

### P2. UpgradePlan::diff full binding

**Do.** Extend `workspace::mounts::upgrade::UpgradePlan::diff` (today compares only the default scheme key) to diff, between pinned and candidate manifests: OAuth endpoints (authorization/token/revocation), default scopes, inject domains and header, flow kind, `token_endpoint_auth`, config field types (added/removed/type-changed), and capability needs (added/widened). Output type: `pub struct UpgradeDelta { pub auth: Vec<AuthChange>, pub config: Vec<ConfigChange>, pub caps: Vec<CapChange>, pub classification: Classification }` with `Classification { Identical, Additive, Breaking, WidensAuthority }`. Tests: one fixture pair per change axis, each asserting the classification.

**Gates.** `GATE-TEST`; a test proves an endpoint-only change classifies `WidensAuthority` (today it classifies identical — that specific regression test is the point of this step).

**PR.** `fix(workspace): UpgradePlan::diff binds the full re-consent surface`

### P3. Mount CRUD endpoints + Registry lock

**Do.**
1. `workspace::mounts::Registry` gains an advisory file lock (fs2, same pattern as the creds store: lock file beside the mounts dir, held across scan-modify-write) — the daemon becomes a second writer here.
2. Daemon routes: `POST /v1/mounts` (body: the `Spec` JSON; validate via parse + `Spec::materialize` against the catalog; write through `Registry::put`; converge that one mount; respond `MountReport { mount, outcome, failure: Option<MountFailure> }`), `DELETE /v1/mounts/{name}` (Registry::remove + converge; 404 `MountNotFound` if absent), `PUT /v1/mounts/{name}` (body: `{ spec: Spec, approved: UpgradeDelta }` — enforcement lands in P4; in P3, `approved` is accepted and stored in the report but not yet enforced, and the PR body says so).
3. CLI: `init` and `mounts rm` call the endpoints when a daemon is running (probe via `ready`), fall back to direct Registry writes when not; `reset` uses DELETE per mount when running. `docs/contracts/50-control-plane.md`: replace the "CLI is the only author" language with the daemon-writes-when-running model, same PR.
4. Credentials never in specs; assert the endpoint rejects any spec whose auth block smuggles a literal secret field (strict parsing already forbids unknown fields; add a test).

**Gates.** `GATE-OPENAPI`, parity, `GATE-TEST`, `GATE-LIVE` plus: `docker exec omnifs curl -s -X POST localhost:7878/v1/mounts -d @providers/dns/dev/mount.json -H 'content-type: application/json'` returns a `MountReport` and the mount serves.

**PR.** `feat(api): mount create/update/delete over the control plane with a locked Registry`

### P4. Daemon-enforced upgrade consent

**Do.** In `MountRuntimes` reconcile and in `PUT /v1/mounts/{name}`: before hot-swapping a changed mount, compute `UpgradePlan::diff` between the running pinned manifest and the incoming one; if classification is `WidensAuthority` (or `Breaking` per the same rule the CLI uses — mirror `crates/omnifs-cli/src/upgrade.rs`'s routing), require the request's `approved` delta to be a superset; otherwise the mount lands in `failed` with `ErrorCode::ConsentRequired` and the actual delta in `detail`. Bare `POST /v1/reconcile` (no approvals) refuses widening swaps the same way. CLI: interactive consent flow computes the delta, shows it, sends it as `approved`.

**Gates.** integration test: write a widened spec to disk, call reconcile, assert the running instance is unchanged (`Arc::ptr_eq` pattern from `reconcile_keeps_running_mount_when_replacement_fails`) and the report carries `ConsentRequired`; then re-call with a covering approval and assert the swap; `GATE-OPENAPI`, parity, `GATE-TEST`.

**PR.** `feat(daemon): capability- and auth-widening upgrades require explicit consent at the swap point`

### P5. Backend identity on the wire

**Do.** Widen `DaemonBackend` to `Native { pid: u32 } | Docker { container_name: String, image: String }` (the daemon learns these from its own environment: pid always; container identity via env vars set by the launcher — add `OMNIFS_CONTAINER_NAME`/`OMNIFS_IMAGE` pass-through in the CLI's docker launch and the entrypoint script). `FrontendInfo.fs_type` becomes `enum FsType { Fuse, Nfs }`. CLI `launch_record` demotes to a cache: written from `DaemonStatus` after a successful launch; the guess-the-container fallback (`backend_from_daemon` defaults) is deleted — when both daemon and record are unavailable, teardown reports "unknown backend" and stops instead of guessing.

**Gates.** `rg -c "fallback|guess" crates/omnifs-cli/src/launch_record.rs` → no fallback construction of container names; `GATE-OPENAPI`, parity, `GATE-TEST`, `GATE-LIVE` then `omnifs down` from the host tears down the right container.

**PR.** `feat(api): backend identity travels on the wire; launch record demotes to a cache`

### P6. Credential health + reload endpoints

**Do.** `GET /v1/credentials` → `Vec<CredentialStatus>` (from A2's `service.health()`; serialization must not include any secret-typed field — add a compile-guard test asserting `CredentialStatus` contains no `SecretString`). `POST /v1/credentials/{id}/reload` → calls `service.reload`, returns the refreshed `CredentialStatus`. `DaemonStatus` gains per-mount `auth_health: Option<CredentialHealth>`. CLI `mounts reauth` (created in C2) calls reload after storing.

**Gates.** `GATE-OPENAPI`, parity, `GATE-TEST`; e2e: rewrite a credential via the service, call reload, assert the next callout uses the new token (FakeAuthServer counter).

**PR.** `feat(api): credential health surface and live reload`

### P7. Providers endpoint

**Do.** `GET /v1/providers` → `Vec<ProviderSummary { name, installed: Vec<version+id-hash>, latest: Option<...> }>` from `workspace::provider::Catalog::{installable, latest_by_name}`. `MountSummary`/`MountDetail` from P3 gain `provider_name` and pinned `provider_id` (content hash hex) — fixing the `provider_id`-holds-a-name wart.

**Gates.** `GATE-OPENAPI`, parity, `GATE-TEST`.

**PR.** `feat(api): provider catalog endpoint and honest provider identity on mount payloads`

### P8. Scoped reconcile + busy signal

**Do.** `POST /v1/reconcile` accepts optional `{ "mounts": ["name", ...] }` (plan/build/remove restricted to those names); when `reconcile_lock` is already held, respond 409 with `ErrorCode::ReconcileBusy` and `Retry-After: 2` instead of queueing.

**Gates.** `GATE-OPENAPI`, parity, `GATE-TEST` including a concurrent-reconcile test asserting the 409.

**PR.** `feat(daemon): scoped reconcile and an explicit busy signal`

### P9. Control-port bearer token `[GATED]`

**Do.** Daemon generates a random 32-byte token at start, writes it to `<config_dir>/control-token` (0600), requires `Authorization: Bearer <token>` on every route except `GET /v1/ready`; the CLI reads the file. Docker mode: the token file lives in the shared workspace mount, so the host CLI reads it naturally. Open the PR with body: "Gated decision: control-port authentication changes the operational contract. Awaiting sign-off." → `blocked-gated`.

**PR.** `feat(daemon)!: bearer-token authentication on the control port`

---

## 9. Phase C: the CLI

### C1. CLI structural fixes

**Do.** In one PR, mechanical and behavior-preserving:
1. Move `daemon_addr()` from `crates/omnifs-cli/src/inspector/source.rs` to a new `crates/omnifs-cli/src/control/addr.rs`; `client.rs`, `launch.rs`, and the inspector import it from there. Use `omnifs_api::default_listen_addr()` (add it to omnifs-api: `pub fn default_listen_addr() -> SocketAddr`) at the four hand-built sites (`daemon/app.rs`, `cli/launch.rs`, `cli/client.rs`, `cli/control/addr.rs`).
2. Backend identity collapse: keep `ConfiguredBackend` (intent) and wire `DaemonBackend` (fact, post-P5); `LaunchBackend` gets ONE constructor `resolve(overrides: BackendOverrides, config: &Config)` replacing `resolve`/`from_config`/`for_backend`; `Runtime` (bollard wrapper) holds a `DockerTarget` instead of copied fields; `DaemonTeardown` gets one `resolve_running_backend()` used by both `down` and `reset_best_effort`.
3. Registry bypasses: `commands/reset.rs` deletes specs via `Registry::remove` (not `fs::remove_file`); `mount_report::artifact_present` calls `Spec::pinned_manifest`.
4. Dissolve `session.rs`: constants → `launch_backend.rs` (or `backend.rs` if renamed), `MountConfig` → `mount_config.rs`, `env_string` → `config.rs`.
5. Fold the adjacent-impl free functions: `mount_tree.rs` renderers into `impl MountTreeData`; `status.rs`'s `collect_status`/`format_mount`/`write_mount_row` into `impl StatusReport`; `host_teardown.rs`'s out-param fns into `impl TeardownSummary`; `auth/mount.rs`'s `mount_auth`/`load_mount_auth` into `MountAuth::{from_spec, load}`; `setup/host_os.rs`'s six fns into `impl HostOs`; `inspector/tree.rs::lookup_node_mut` into `impl PathNode`; doctor's ten probes onto a `struct Doctor` context.
6. `thiserror`: back `HintedError`'s Error impl with it, or remove the dependency — pick: use it (it is declared).

**Gates.** `rg -c "fn daemon_addr" crates/omnifs-cli/src/inspector` → 0; `rg -c "fs::remove_file" crates/omnifs-cli/src/commands/reset.rs` → 0; `GATE-TEST`, `GATE-HOST`.

**PR.** `refactor(cli): fact ownership, backend-identity collapse, registry discipline, method hygiene`

### C2. Grammar, --json, exit codes, hints

**Do.**
1. Remove `mounts add` (delete the forward; `init` is canonical). Add `mounts reauth <name>` (the body of today's `init --reauth`, which is then deleted along with the positional repurposing). Add `providers ls` (renders the catalog; local, plus daemon view when P7 is up).
2. `--json` on `status` (exists), `doctor`, `mounts ls`, `providers ls`, `version` — each serializing `omnifs-api` types or thin CLI structs derived from them.
3. Exit-code contract, implemented in `main.rs`'s single exit path: 0 success; 1 failure; 2 clap usage; 3 daemon unreachable (`ErrorCode::DaemonShuttingDown` or connect failure); 4 auth/consent required (`AuthRequired`/`ConsentRequired`); 5 degraded (command succeeded, health shows a degraded mount). Document in `--help` epilogue.
4. Output discipline: data on stdout, progress/notices on stderr; color/spinners only when `stdout` is a TTY and `NO_COLOR` unset.
5. Nudge: any state-touching command checks health (daemon or local) and prints at most one stderr line per 24h (`<config_dir>/telemetry/last-nudge` timestamp) naming the fix command.
6. `doctor` gains the live section: query `/v1/status` + `/v1/credentials`, correlate `failed` mounts and non-Ready credentials into per-mount verdicts each ending with `fix: <command>`.
7. `omnifs` bare invocation: configured → compact status dashboard; unconfigured → onboarding pointer to `omnifs setup`.

**Gates.** `GATE-TEST`; a CLI integration test per exit code (kill the daemon → 3; missing credential on an auth-requiring op → 4); `omnifs mounts add` no longer parses (`cargo run -p omnifs-cli -- mounts add x; echo exit=$?` → 2).

**PR.** `feat(cli)!: canonical grammar, machine contract (--json + exit codes), doctor triage, nudges`

### C3. Setup wizard + express init

**Do.** Implement `docs/future/cli-experience.md` §"The setup wizard, specified" exactly:
1. Extract shared stage functions (environment check, runtime selection, mount-point resolution, spec creation, auth, launch, verify-first-read) into top-level CLI modules; `setup` becomes narration + sequencing over them; `init` becomes the compressed express lane over the same functions.
2. Wizard: 6 numbered stages as specified (orientation text; environment with inline fixes and wait-for-enter; runtime with consequence-terms copy; mount point; provider picker flagging credential-free providers; guided per-provider init; launch + a REAL `ls` of the new mount displayed; graduation card with the three daily commands and a completions offer).
3. Re-run = review mode: configured system → show current settings, offer change/add/re-check, no first-run script.
4. `init` on a fresh machine: apply defaults with the one-line notice + "For the guided tour, run `omnifs setup`", then continue; `init` ends by verifying a root listing of the new mount before printing success, and prints `try: ls <path>`.
5. Every prompt has a flag; `--no-input` fails fast naming the missing flag; `setup -y` end-to-end accepted defaults still works.

**Gates.** `GATE-TEST`; scripted non-interactive checks: `omnifs setup -y --no-up; echo exit=$?` → 0 in a temp `OMNIFS_HOME`; `omnifs init test --no-input --yes` against the fixture provider mounts and verifies; wizard flag-coverage test: walk the clap tree and assert every interactive prompt site has a corresponding flag (maintain a static table in the test; it must be updated when prompts are added — the test failing on a new prompt is the point).

**PR.** `feat(cli): the guided setup wizard and the express init lane over shared stages`

### C4. Golden-path e2e (K2 instrumentation)

**Do.** `crates/omnifs-cli/tests/golden_path.rs` (or extend `lifecycle_acceptance.rs`): in a temp `OMNIFS_HOME`, run `setup -y --no-up` → `init test --no-input --yes` → `up --wait 30s` (implement `--wait <dur>`: poll `/v1/ready` until 200 or deadline) → read a known fixture file through the mount → `down`. Record wall-clock; write it into the telemetry dir; the test asserts completion < 120s and exit codes per contract. Wire into the CI lane that can mount (Linux FUSE in CI, or mark macOS-NFS-local with the serialization lock).

**Gates.** the test passes 3 consecutive runs locally; `GATE-TEST`.

**PR.** `test(cli): golden-path e2e measuring install-to-first-cat`

---

## 10. Phase G: configuration

### G1. One inheritance function

**Do.** Make `Spec::apply_provider_metadata` (workspace) the only manifest-defaults fold: `commands/init/spec_creation.rs` (`MountSpecCreator::create`), `auth/mount.rs` (`AuthSelection::from_provider_default`), and `upgrade.rs` (`apply_additive_upgrade`) all call it (extending its signature if needed to cover the additive-upgrade fold — one function, parameterized, not three). Delete the shadow logic. itest already calls it; keep.

**Gates.** `rg -c "config_metadata.defaults\(\)" crates/omnifs-cli/src` → 0 outside the shared function; `GATE-TEST`, `GATE-LIVE` (`omnifs init dns` produces a byte-identical spec to before — snapshot-test the generated spec for the dns provider).

**PR.** `refactor(workspace): Spec::apply_provider_metadata is the sole manifest-defaults implementation`

### G2. Atomic writes + precedence + single-owner constants `[PARALLEL-OK]`

**Do.** (1) Route `ConfigFile::save`, `Registry`'s spec writes, and the launch-record write through `workspace::io::write_atomic`. (2) `resolve_setting` helper in `cli/config.rs`: `fn resolve<T>(flag: Option<T>, env: &str, from_config: impl Fn(&Config) -> Option<T>, default: T) -> T` with env parsing; migrate the container-name/image and daemon-addr resolvers onto it. (3) Delete the dead `WorkspaceLayout::wasm_cache_dir` duplicate pair by delegating engine's to workspace's. (4) One `DEFAULT_DAEMON_LOG_LEVEL` constant with an explicit foreground/spawned parameter replacing `"warn"`/`"info"` literals. (5) Expose `workspace::layout::resolve_mount_point` (env-honoring) and use it from both the daemon context and setup's preview.

**Gates.** `rg -c "std::fs::write" crates/omnifs-cli/src/config.rs` → 0; `rg -c "\"warn\"|\"info\"" crates/omnifs-cli/src/main.rs crates/omnifs-cli/src/launch_backend.rs` → only the constant definition; `GATE-TEST`.

**PR.** `refactor(config): one atomic writer, one precedence chain, single-owner constants`

### G3. Inject-domain ⊆ capability-need validation

**Do.** In `workspace::provider::manifest` validation (and mirrored in the macro's emission checks if cheap): every `inject_domains` entry on every scheme must be covered by a declared domain access-need; error names the scheme and domain. Fix any provider this catches in the same PR.

**Gates.** a fixture manifest with an uncovered inject domain fails validation with the named error; `GATE-PROVIDERS`, `GATE-TEST`.

**PR.** `feat(workspace): inject domains must be covered by declared capability needs`

### G4. Guest-path CI grep + Dockerfile dedup `[PARALLEL-OK]`

**Do.** (1) `scripts/ci/check-guest-paths.sh`: greps the five `/root/.omnifs` sites and three `/omnifs` sites (list them explicitly in the script with file paths), fails if any site's literal diverges or disappears; wire into CI. (2) Extract the duplicated runtime stage between `Dockerfile` and `scripts/ci/Dockerfile.runtime`: shared base via a named stage in one file consumed by the other (`docker buildx` context) OR, if the build graph forbids it, add a loud sync banner comment in both plus a CI check that the apt package lists match (`diff <(grep -A20 'apt-get install' Dockerfile | sort) <(...)` shape). Cross-reference both files in `docs/contracts/60-build-validation.md`.

**Gates.** the new scripts exit 0 on the current tree and non-zero when a literal is edited (test by temporary mutation locally, revert); `GATE-DOCS`.

**PR.** `ci: tripwires for guest-path literals and runtime-image duplication`

### G5. dev.ts through the CLI

**Do.** Replace `scripts/dev.ts`'s hand-rolled spec/credential writers (`writeJson` for `mounts/*.json` and `credentials.json`) with invocations of the built CLI: `omnifs init <provider> --no-input --yes --home <devhome> ...` per pinned dev mount (flags from C3), and credential seeding via the CLI's token intake (`--token-env`). The seven `providers/*/dev/mount.json` fixtures become generation inputs only (config overrides), not verbatim spec files; regenerate on each `just dev`.

**Gates.** `just dev -y` produces a working shell with all profile mounts serving; `rg -c "credentials.json" scripts/dev.ts` → only path references, no direct writes.

**PR.** `refactor(dev): dev home rendered through the CLI, not hand-rolled writers`

---

## 11. Phase F: projection and frontends

### F1. FUSE async-first dispatch `[LINUX]`

**Do.** In `crates/omnifs-fuse/src/filesystem.rs`: every `fuser::Filesystem` callback clones its needed `Arc`s, moves the owned `Reply*` object into `self.rt.spawn(async move { ... reply.ok/…(...) })`, and returns immediately — no `block_on` remains. Bound in-flight ops with a `tokio::sync::Semaphore` (64 permits) acquired inside the task before the tree call. Preserve `opendir`-snapshot/`readdir`-window semantics exactly. Un-ignore N3's FUSE variant (delete `#[ignore = "enabled by F1"]`).

**Gates.** `rg -c "block_on" crates/omnifs-fuse/src` → 0; N3's FUSE concurrency test green ON LINUX; `GATE-HOST` on Linux; `GATE-LIVE` including `tail -f` on a live file and `find /omnifs -maxdepth 3` during a slow read.

**PR.** `perf(fuse)!: async-first dispatch; a cold provider op no longer blocks the mount`

### F2. engine::render toolkit + frontend migration `[LINUX]`

**Do.**
1. Implement in `crates/omnifs-engine/src/render/`: `identity.rs` (`IdentityTable<Id: Copy+Eq+Hash, Body>` wrapping the `DashMap<Key, Id>` + `DashMap<Id, Entry>` pair with `get_or_alloc(key, mk)` using the entry-API atomic pattern and the two merge rules: provider resolution overrides a synthetic marker and never the reverse; an exact/learned size wins over a stale non-exact one — lift the logic verbatim from `omnifs-fuse/src/inode.rs:241-289` and `omnifs-nfs/src/adapter.rs:334-400`, recording a field-by-field diff of the two `NodeEntry` structs in the PR body); `follow.rs` (`FollowSizeTable`: `grow(id, size)` keeping the max, `get`, `remove`); `invalidate.rs` (`fn stale_ids<I>(report: &InvalidationReport, table: &IdentityTable<I, B>) -> Vec<I>` matching paths and prefixes); `attrs.rs` (shared dir/symlink/file classification + read-only mode bits from the two `attr_from_metadata` twins).
2. Add `TreeErrorKind::retry_class() -> RetryClass { Retry, Gone, Terminal, TooLarge }` in `tree/error.rs`, encoding the partition both frontends currently hand-roll; frontends map class → errno/nfsstat only.
3. Migrate both frontends onto all four pieces; their `NodeEntry` types become wrappers adding only `scope`/`parent`/`size_exact` (NFS) and nothing (FUSE beyond the body enum). Delete the originals, including the duplicated `split_parent_leaf`/`parent_child_for_notify` pair inside omnifs-fuse.

**Gates.** `rg -c "attr_from_metadata" crates/omnifs-fuse crates/omnifs-nfs` → ≤1 each (thin wrapper allowed, no logic); N2/N3 nets green; NFS live tests green on macOS; FUSE tests green on Linux; `GATE-LIVE`.

**PR.** `refactor(render): shared identity/follow/invalidation/attr toolkit; frontends keep only protocol`

### F3. Shared materialize cap

**Do.** Move the 64 MiB whole-file materialization budget into `Tree::read`/`open` as `pub const MATERIALIZE_MAX_BYTES: u64 = 64 * 1024 * 1024` in `engine/src/render/attrs.rs`; exceeding it yields `TreeErrorKind::TooLarge`; NFS deletes `enforce_materialize_cap` + `OPEN_MATERIALIZE_MAX_BYTES` and maps `TooLarge`→`Resource`; FUSE maps `TooLarge`→`EFBIG`. Ranged-capable files are unaffected (the cap applies to full-materialize paths only — mirror the exact condition NFS uses today).

**Gates.** a tree-level test: a fixture file declaring exact size > cap fails with `TooLarge` on full read and succeeds via ranged read; NFS's own cap tests repointed; `GATE-TEST`, `GATE-LIVE`.

**PR.** `feat(tree): one materialization budget for every frontend`

### F4. Pagination projection face into tree

**Do.** Move `CTRL_NEXT`/`CTRL_ALL`/ignore-file constants + `control_entries()` from `engine/src/pagination.rs` (post-M4 path) into `engine/src/tree/synthetic.rs` (which currently re-imports them); the runtime keeps only the raw fetch-next-page primitive and the cache-backed accumulation. Public re-exports unchanged for frontends (they consume tree's synthetic surface already).

**Gates.** N2's `pagination_exhaustive.rs` green unmodified; `rg -c "CTRL_NEXT|CTRL_ALL" crates/omnifs-engine/src/pagination*` → 0 (definitions live in tree/synthetic).

**PR.** `refactor(tree): the synthetic pagination surface is owned by projection, not the runtime`

### F5. ServingContext + Mounts::Single removal

**Do.** New `engine/src/serving.rs`: `pub struct ServingContext` with constructors `from_runtimes(Arc<MountRuntimes>)` and `single(mount: String, runtime: Arc<Runtime>)`; move `split_mount_path`, root-mount claiming, and mount enumeration from `tree/mod.rs` onto it; `Tree::new(ctx: ServingContext)` replaces both old constructors; delete the `Mounts` enum. It carries NO policy and NO scope claims (doc comment states this and points at the Worldview graduation rule). Update itest/conformance callers.

**Gates.** `rg -c "Mounts::Single|for_runtime" crates/` → 0; conformance + `GATE-TEST` + `GATE-LIVE`.

**PR.** `refactor(engine): Tree serves a ServingContext; the test-only mount variant dies`

---

## 12. Phase S: SDK

### S1. SDK P0: route export, error context, version evidence, event drops

**Do.**
1. `Router::routes() -> Vec<RouteDescriptor>` where `#[derive(Serialize)] pub struct RouteDescriptor { pub template: String, pub kind: RouteKind, pub object_kind: Option<String>, pub captures: Vec<CaptureDescriptor { name, type_name, choices: Option<Vec<String>> }> }` and `RouteKind { Dir, File, Treeref, Object, FileObject, Alias, Collection }` — populated at seal time from the registration tables in `router/register.rs`.
2. Error context: dispatch wraps every handler/load error with operation + path (`ProviderError::with_context(op: &str, path: &Path)` appending a structured suffix, preserving `ProviderErrorKind`); guest panics caught at the export boundary report the route template.
3. Version evidence: the `#[provider]` macro embeds the WIT package string (read it from `crates/omnifs-wit/wit/provider.wit`'s `package` line at macro-build time or hardcode + a parity test against the wit file) and the SDK crate version into the metadata custom section; `workspace::provider::sections` parses and exposes them.
4. Event drops: the macro's non-timer `provider-event` fallthrough logs at warn with the event kind instead of silently returning empty effects.
5. Verify P0.8 is fixed: write a conformance test chaining `o.file("x").lazy().project(f)`-shaped face registration and asserting the flag lands on the CORRECT leaf; if it fails, fix the pending-flag application in `router/object.rs` in this PR.

**Gates.** `GATE-PROVIDERS`, conformance, tracer smoke (`GATE-LIVE` with github+linear+docker profile); a test asserts `routes()` output for the test provider matches a checked-in snapshot; `GATE-SCHEMA` (version-evidence fields are additive; commit regeneration).

**PR.** `feat(sdk): route-table export, error context, contract-version evidence, no silent event drops`

### S2. router/object.rs split + ServeCtx + typed ObjectKind

**Do.** Split the 2,049-line `router/object.rs` into `router/object/{spec.rs,dispatch.rs,serve.rs}` (registration/faces; lookup/list/read dispatch; the serve pipeline). Give `ServeCtx` an impl with its four serve functions as methods. Store `ObjectKind` (the newtype) in registration tables and compare it typed at seal (`register.rs:376`'s raw `&str` comparison dies). Behavior-identical; conformance green unmodified.

**Gates.** `wc -l crates/omnifs-sdk/src/router/object/*.rs` — no file > 900; `rg -c "kind_str" crates/omnifs-sdk/src` → 0; `GATE-PROVIDERS`, conformance, tracer live smoke.

**PR.** `refactor(sdk): object route module split, ServeCtx methods, typed object kinds`

### S3. Macro unification + helpers/pattern cleanup

**Do.** Convert `endpoint_macro.rs` to the Args-struct + `expand()` shape the other macros use (killing the 174-line function); extract the 4× duplicated generic-argument helper into one shared macro-crate util; dissolve `omnifs-sdk/src/helpers.rs` (`err()` deleted in favor of the existing `From`; `pretty_json` → `repr.rs`); replace `router/pattern.rs`'s `Result<T, String>` channel with a `PatternError` enum, deleting the three adapter fns it forces.

**Gates.** `rg -c "Result<.*, String>" crates/omnifs-sdk/src/router/pattern.rs` → 0; `GATE-PROVIDERS`, conformance.

**PR.** `refactor(sdk-macros): one macro shape; typed pattern errors; helpers dissolved`

### S4. DirProjection removal + two-stage registration

**Do.** Remove `DirProjection` from the public surface with Linear migrated to `Collection<Issue>` (the collection API exists; the migration is the tracer). Replace the `Rc<RefCell<Option<Rc<...>>>>` late-binding cell with two-stage registration: declarations hold unresolved data until `seal`, which constructs plain `Rc<ResolvedChildView>`; merge the parallel `collections`/`collection_handlers` lists (string-matched today) into one declaration type.

**Gates.** `rg -c "DirProjection" crates/ providers/` → 0; `rg -c "RefCell<Option<Rc" crates/omnifs-sdk/src` → 0; `GATE-PROVIDERS`, conformance, tracer live smoke including Linear.

**PR.** `refactor(sdk)!: Collection is the one listing surface; two-stage registration replaces the late-binding cell`

---

## 13. Phase L: legibility, staleness, providers

### L1. README/schema synthesis (D10)

**Do.** SDK affordance over S1's `routes()`: at seal, synthesize per provider root (and per top-level literal branch) a `README.md` leaf describing the keying schema, the route templates with capture explanations (from `RouteDescriptor`), and 3 example commands — generated text, one template, no per-provider prose (providers may override with their own registered file). Serve them as ordinary synthetic leaves hidden from `find`/`grep -r` the same way pagination control files are (the ignore-file mechanism), so `cat README.md` works but bulk tools are not polluted. Enable for all providers; snapshot-test the github root README.

**Gates.** `GATE-LIVE`: `docker exec omnifs cat /omnifs/github/README.md` prints the synthesized text; `docker exec omnifs /bin/zsh -lc 'grep -r "keying schema" /omnifs/github | wc -l'` → 0 (ignore mechanism works); conformance + tracer smoke.

**PR.** `feat(sdk): self-describing provider trees — synthesized README leaves from the route table`

### L2. omnifs usage skill

**Do.** `skills/omnifs-usage/SKILL.md`: how an agent navigates a mount (ls-first discovery, README leaves, `@next`/`@all` semantics, freshness expectations, what NOT to do: no `find /` from the root, no write attempts). Keep under 200 lines; include 5 worked one-liners. Add an `omnifs skill install claude-code` CLI command that copies it into `~/.claude/skills/` (or prints the path if the harness is absent).

**Gates.** skill file lints against the repo's skill conventions (`ls skills/` for the existing provider-sdk skill as the template); CLI command round-trips in a temp HOME.

**PR.** `feat(skills): the omnifs usage skill and its installer`

### L3. Staleness backstop

**Do.** Host-driven revalidation for lazy providers: in `MountRuntimes`' existing per-mount timer scaffolding, add a revalidation tick (default interval from the manifest's refresh field when present, else 15 minutes, constant named `DEFAULT_REVALIDATE_SECS`) that re-drives the provider's conditional-load path for the N most-recently-read objects per mount (N=32; track recency in the engine read path with a small LRU of LogicalIds) and applies resulting invalidations. Providers with validators get real conditional requests; providers without get a bounded refresh. Config kill-switch per mount spec: `"revalidate": false` (additive spec field, strict-parse compatible).

**Gates.** integration test with the fixture provider: change upstream fixture bytes, advance the paused clock past the tick, assert a subsequent read returns fresh bytes without an explicit invalidation from the provider; `GATE-TEST`, `GATE-LIVE`.

**PR.** `feat(engine): host-driven revalidation backstop for lazily-invalidating providers`

### L4. web provider

**Do.** `providers/web`: routes `https/{host}/{*rest}` (a readable file: the fetched page passed through a readability extraction to Markdown — extraction runs provider-side in wasm; pick a pure-Rust readability crate that compiles to wasm32-wasip2, e.g. `dom_smoothie` or `readability` — verify compilation before committing to one, and if none compiles, STOP and report options) and `raw/https/{host}/{*rest}` (verbatim bytes). Capability needs: a wildcard-domain grant is a GATED decision — instead declare the provider with a mount-config `domains: Vec<String>` (host-resource-free config) that materializes into domain grants at init, so each mount enumerates its allowed sites. Include the provider README and dev fixture.

**Gates.** `GATE-PROVIDERS`, conformance (new fixture tests with a local fixture HTTP server), `GATE-LIVE`: `cat /omnifs/web/https/example.com/` renders text.

**PR.** `feat(providers): web provider — any allowed URL as a readable file`

### L5. GitHub deepening

**Do.** Extend `providers/github` with, in priority order: PR files/diff (`pulls/{n}/files/`, `pulls/{n}/diff.patch`), PR reviews + review comments (`pulls/{n}/reviews/`), check runs (`pulls/{n}/checks/`), and notifications (`notifications/`). Each as objects with canonical payloads and rendered Markdown representations, following the existing issue/PR patterns in the provider. Respect the list-vs-GET byte-identity rule: never seed an object canonical from a list response (list-embedded objects differ byte-wise; only rendered views may come from lists).

**Gates.** conformance fixtures per new route; tracer live smoke: `cat` a real PR diff and a review thread through the mount; `GATE-PROVIDERS`, `GATE-LIVE`.

**PR.** `feat(github): PR diffs, reviews, checks, and notifications as files`

---

## 14. Phase W: worldview v0 and the replica

### W1. Worldview v0 `[GATED]`

**Do.**
1. Format: `<config_dir>/worldviews/<name>.json`, strict serde: `{ "name": string, "mounts": [ { "mount": string, "subtree": optional absolute path string, "read_only": true } ] }` (`read_only` must be `true` in v0; parsing rejects `false`).
2. Enforcement: `ServingContext::from_worldview(runtimes, &Worldview)` filters the mount set and carries per-mount subtree prefixes; `split_mount_path` and every resolve/list/read/open path returns NotFound for anything outside a prefix (tests on all four op paths — this is the graduation rule from the rework plan: scope enforced on EVERY serving path or the name Worldview does not ship).
3. CLI: `omnifs up --worldview <name>`; the daemon records the active worldview in `DaemonStatus`.
4. This step graduates `ServingContext` toward the `Worldview` name; open the PR flagged: "Gated: introduces the Worldview concept with enforced read-only scoping; per the plan this requires the scope rule to be enforced on every serving path — evidence in tests listed below." → `blocked-gated` until sign-off.

**PR.** `feat(engine)!: worldview v0 — enforced read-only scoping of the served namespace`

### W2. Replica snapshot + export

**Do.**
1. Engine cache API: enumerate canonical entries per mount (`cache::object` iteration with LogicalId + path + bytes handle) — sanctioned, read-only.
2. `omnifs snapshot <mount> --out <dir>`: exports the mount's canonical store as a plain directory tree of rendered canonical files plus an index.json (logical id → path → blake3), via the daemon (`GET /v1/mounts/{name}/export` streaming a tar) when running, direct cache read when not.
3. Document that `diff -r` between two snapshot dirs is the audit story; add the demo script `scripts/demo/snapshot-diff.sh`.

**Gates.** e2e: snapshot the fixture mount twice around an upstream change; `diff -r` shows exactly the changed file; `GATE-OPENAPI` (new route), parity, `GATE-TEST`.

**PR.** `feat(replica): mount snapshots and canonical-store export`

---

## 15. Horizon `[HORIZON — do not execute]`

Liveness (subscription callout family, event pump, watch plumbing), the MCP bridges (both directions), the inspector web dashboard, devcontainer/CI packaging, and any provider beyond L4/L5 are deliberately NOT planned here. They are re-planned by Raul after T4's M0 report is reviewed, because the strategy prices them against those numbers. If you finish everything above, stop.
