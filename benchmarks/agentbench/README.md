# Agentbench: the K1 instrument

Agentbench measures whether projecting external data as a filesystem (omnifs) lets an agent do the same work with fewer tokens and comparable success than the same data behind a tool-call MCP baseline. Same tasks, same model, two conditions.

The deterministic v1 runs against a fixed fixture corpus so it is repeatable and free of upstream flakiness. The live variant (real GitHub and Linear providers, official vendor MCP servers) is T4 and spends money, so it is gated behind explicit approval.

## Conditions

- **mount**: the fixture corpus served as a real filesystem through omnifs. The agent runs with `cwd` set to the mounted root and is allowed the file tools plus bash (`Read`, `Grep`, `Glob`, `Bash`).
- **mcp**: the same bytes behind the structurally-honest MCP baseline in `mcp-baseline/server.ts`, a Bun stdio MCP server exposing exactly two tools, `list_dir(path)` and `read_file(path)`. The agent is restricted to those two tools.

Both conditions run the identical prompt and model; only the data-access surface differs.

## Layout

- `tasks/*.yaml` — one task per file.
- `fixture-data/` — the generated corpus (github-like and linear-like trees).
- `gen-fixture.ts` — deterministic corpus generator.
- `FIXTURE-FACTS.md` — the answer key and planted facts, kept OUTSIDE `fixture-data/` so an agent reading the corpus never sees it.
- `mcp-baseline/server.ts` — the two-tool MCP baseline for condition B.
- `runner.ts` — runs task x condition, invokes the model, grades, writes the report.
- `grade.ts` — `contains` / `regex` / `judge` grading.
- Reports land in `../reports/agentbench-<date>.{json,md}`.

## Task schema

One task per YAML file:

```yaml
id: corr-001
family: correlation   # correlation | navigation | aggregation | reconstruction
prompt: "<the user prompt given to the agent verbatim>"
success:
  type: contains      # contains | regex | judge
  value: "<string, regex source, or judging rubric sentence>"
max_turns: 30
```

Notes on the fields, so a step is executable without any other document:

- `prompt` is passed verbatim to the model. Prompts reference paths relative to the dataset root (for example `github/repos/acme-api/issues/3/body.md`) so both conditions can answer.
- `success.type: contains` grades a case-insensitive substring match of `value` against the agent's final answer.
- `success.type: regex` grades `new RegExp(value).test(answer)`. Write regex values single-quoted in YAML so backslashes stay literal (for example `value: '\b4\b'`).
- `success.type: judge` recognizes the type but requires an extra model call to grade, which costs money and is gated to T4. The runner refuses to run a judge task unless `--allow-judge` is passed, and even then the judge model call is not implemented in this build.
- `max_turns` is a soft cap. The current Claude CLI has no turn-limit flag (see the drift note below), so a run whose reported `num_turns` exceeds `max_turns` is failed after the fact.

## Fixture corpus

Regenerate deterministically:

```bash
bun gen-fixture.ts
```

This writes ~195 small files under `fixture-data/`:

- github-like tree: `github/repos/<repo>/issues/<n>/body.md` and `.../comments/<c>.md`, across `acme-api`, `acme-web`, `acme-cli`, plus a per-repo `README.md`.
- linear-like tree: `linear/issues/<KEY>/{issue.md,activity.md}` across teams ENG, OPS, DES, plus `linear/teams.md`.

Planted facts drive the tasks: cross-references between the two trees for the correlation family, and numeric counts for the aggregation family where SQL or a query tool should win. Keeping the aggregation family in the suite keeps the benchmark honest: files are not the right tool for every question, and the report shows where they lose. The answer key is in `FIXTURE-FACTS.md`.

## Running

Dry-run smoke (no model call, no cost):

```bash
bun runner.ts --tasks corr-001 --dry-run
```

Dry-run substitutes a canned transcript for the model call, so it proves the whole pipeline (task loading, invocation shape, grading, aggregation, report) without spending money. The report is written with `dry_run: true` and canned placeholder numbers.

Full runner flags:

```
--tasks <ids>          comma/space list of task ids (default: all)
--conditions <list>    subset of "mount,mcp" (default: both)
--mount-root <path>    cwd for condition mount (default: fixture-data)
--fixture-root <path>  dataset root for the MCP baseline (default: fixture-data)
--model <id>           model passed to the Claude CLI (default: opus)
--out <file>           report path (default: ../reports/agentbench-<date>.json)
--dry-run              skip the model call; use a canned transcript
--allow-judge          permit judge-type tasks (gated to T4; costs money)
--claude-bin <path>    Claude CLI binary (default: claude)
--max-turns <n>        override the per-task soft turn cap
```

A real (non-dry) run invokes the model and spends tokens. That is T4's approved territory; do not run it in T3.

## Serving the corpus through omnifs (condition mount)

Condition `mount` needs the corpus served as a filesystem through omnifs, with `--mount-root` pointing at that mount. The runner does not stand up the mount; the operator provides it.

There is no turnkey way to serve an arbitrary host directory through the test provider today, and adding one is a gated decision. A `TreeRef` (the host handle that becomes a served subtree) can only be minted by the host's internal registry, which is reachable solely through the git-clone and archive-extract callouts. A provider `preopen` puts bytes into the guest sandbox for `std::fs`, but a preopen is not a served subtree, so a treeref route cannot turn a preopened host directory into a bind mount. Wiring a config-driven treeref of an arbitrary host path would require a new host import plus a capability authorizing it, which changes the provider-authority security model and is a gated decision (see the repo AGENTS.md gated decisions). It is deliberately out of scope for T3.

Two non-gated ways to serve the corpus at a real mount, both using existing host plumbing, if a live run is wanted before T4 lands a proper path:

- archive-blob: tar the corpus, seed the archive into the host blob cache, and have a treeref route open it via the archive callout, which serves the extracted tree as a real bind mount.
- preopen plus projection: declare a `preopened_path` capability and a dir config field, then write provider routes that walk the preopened directory and project each entry through the normal read path.

Until one of those is wired, `--mount-root` can point at any directory an operator has arranged omnifs to serve (or, as a degenerate non-omnifs baseline, at `fixture-data/` directly). Serve only the corpus root so parent files (including `FIXTURE-FACTS.md`) are not reachable.

## Claude CLI flags (probe first; they drift)

Probed against Claude CLI 2.1.199 with `claude --help`. Record what you find before a live run; these flags change between releases.

- Print mode with JSON: `claude -p "<prompt>" --output-format json`.
- Model selection: `--model <alias|full-id>`.
- Tool restriction to the built-in set: `--tools Read,Grep,Glob,Bash` (condition mount).
- MCP baseline: `--mcp-config <file> --strict-mcp-config --tools mcp__fixture__list_dir,mcp__fixture__read_file` (condition mcp). The MCP config names the server `fixture`, so its tools are `mcp__fixture__<tool>`.
- Headless permissions: `--permission-mode bypassPermissions`.
- There is no `--max-turns` flag in 2.1.199. A task's `max_turns` is enforced after the fact against the JSON result's `num_turns`.

The print-mode JSON result carries `result` (the answer text), `num_turns`, `duration_ms`, `usage.{input,output,cache_creation_input,cache_read_input}_tokens`, `total_cost_usd`, and a `modelUsage` map keyed by the resolved model id. The runner sums the usage fields for `tokens_total` and records the resolved model id from `modelUsage`.

## Report

`runner.ts` writes `../reports/agentbench-<date>.json` and a `.md` summary beside it. The JSON carries per-task rows `{id, family, condition, success, tokens_total, turns, wall_ms}`, per-family aggregates, and the two headline numbers K1 cares about:

- `total_token_ratio`: overall mount tokens divided by overall mcp tokens (below 1.0 means the mounted tree used fewer tokens).
- `success_delta_pp`: mount success rate minus mcp success rate, in percentage points.

## Model version pinning

Pin the exact model id and record it in the report. The default `--model opus` alias resolves to `claude-opus-4-8` at the time of writing; pass `--model claude-opus-4-8` for a pinned run, and read `resolved_model` back from the report (populated from the CLI result's `modelUsage` on real runs). Do not compare reports produced under different `resolved_model` values.
