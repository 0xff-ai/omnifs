# Token-efficiency and wall-clock eval framework

Status: proposed
Scope: `evals/` (new top-level), no host or provider changes required

## Goal

Quantify whether mounting a service through omnifs gives an LLM agent a
measurable advantage over the agent's default access patterns and over
purpose-built skills, on the same information needs. Two questions drive
the design:

1. **Tokens.** How many input/output/cached tokens does the agent spend
   to satisfy a task in each access mode?
2. **Wall clock.** How long does the task take end-to-end, including
   model latency, tool latency, and round-trip count?

The framework treats omnifs as a hypothesis under test, not a foregone
conclusion. A mode wins or loses on the numbers; the table is the
deliverable.

## Modes under test

A "mode" is a tool surface the agent is allowed to use. The same agent
loop, same model, same task prompt, same ground-truth grader. Only the
allowed tools change.

| Mode | GitHub access | arXiv access | Notes |
|------|---------------|--------------|-------|
| `omnifs` | mount at `/mnt/gh/{owner}/{repo}/...`; `cat`, `ls`, `grep`, `find`, `rg` via Bash | mount at `/mnt/arxiv/{papers,categories}/...`; same shell toolbox | host cache on. Deny `gh`, `curl`, `WebFetch`, `WebSearch` |
| `builtin` | `gh` CLI + `curl` against api.github.com + `WebFetch` | `WebFetch` + `WebSearch` | no mount visible. Closest to "what the agent does today out of the box" |
| `skill` | a dedicated skill that wraps the GitHub access pattern (e.g. a `github-read` skill that exposes `get-issue`, `get-pr-diff`, `list-prs`, ...) | a dedicated `arxiv-read` skill | the skill internally uses whichever backend it prefers (the MCP github tools, the arXiv API). Deny mount, deny raw HTTP |

Mode is enforced by a per-run tool allowlist and a deny matcher on `Bash`
commands (regex against `gh `, `curl http`, etc.). The harness records
any blocked attempts; a run that hits a deny is not invalidated, but the
event is logged because it tells us how the agent *wanted* to solve the
task.

## Task corpus

The corpus is a checked-in JSON file of tasks. Each task has a frozen
target (issue number, PR sha, paper id, snapshot date), a prompt
template, and a grader. Three tiers per service so we can see where each
mode's advantage shows up.

### GitHub tasks

Tier 1 — **single read** (1 artifact, no exploration):
- "What is the title of issue #N in `{owner}/{repo}`?"
- "What state is PR #N in?"
- "Return the exact text of the top-level body of issue #N."
- "How many files changed in PR #N?"

Tier 2 — **multi read** (2–5 related artifacts, no search):
- "Summarize the discussion on issue #N (body + all comments)."
- "List the files touched by PR #N and the net additions per file."
- "Quote the review comment from `@user` on PR #N that mentions
  `<token>`."

Tier 3 — **exploration** (the set of artifacts to read is not given):
- "Find the most recently closed PR in `{owner}/{repo}` that touches
  `crates/host/src/runtime/` and return its sha."
- "Across the last 20 open issues, how many mention `wasm`?"
- "Which open issue has the most comments? Return its number and count."

### arXiv tasks

Tier 1 — **single read**:
- "What is the abstract of `{arxiv-id}`?"
- "Who are the authors of `{arxiv-id}`?"

Tier 2 — **multi read**:
- "List the titles of the 5 most recent submissions in `cs.LG` on
  `{frozen-date}`."
- "For each of these three arXiv ids, return the primary category."

Tier 3 — **exploration**:
- "Find a paper submitted to `cs.DC` on `{frozen-date}` that has
  `consensus` in its title. Return id and abstract."

### Freezing the targets

Every target id is pinned (issue #, PR sha, paper id, snapshot date).
The grader compares against a captured ground-truth artifact, not
against a live API, so the score is reproducible even if upstream
state changes. The harness ships a `corpus/snapshot/` directory with
the captured payloads; the grader loads from there.

## Metrics

Captured per turn and aggregated per run:

- **Tokens**: input, output, cache-create, cache-read. Source: SDK
  `usage` events. Reported as raw counts and as cost (model price
  table is a small JSON).
- **Wall clock**: total seconds from the first user turn to terminal
  assistant turn. Also: time-in-tool vs time-in-model split.
- **Tool calls**: count + per-tool breakdown. For `omnifs` mode this
  is mostly `Bash`; for `builtin` it is a mix; for `skill` it is one
  or two skill invocations.
- **Round trips**: number of model turns.
- **Outcome**: pass / fail / partial, scored by the task grader.
- **Quality**: for free-form answers, an exact match where possible,
  a substring match for quotes, and an LLM-judge fallback with a fixed
  rubric for summaries. The judge is a separate model call, recorded
  but not counted against the run's token budget.

Secondary:

- **Cache hit rate** (omnifs): from host metrics, reported alongside
  but not used for scoring (so warm-cache wins are visible without
  being smuggled in).
- **Denied-tool attempts**: how many times the agent reached for a tool
  the mode forbids before settling on an allowed one. A high count is
  a signal that the mode is unergonomic for the task, even if the
  outcome is correct.

## Run shape

```
for task in corpus:
  for mode in [omnifs, builtin, skill]:
    for trial in 1..N:                         # variance budget
      for cache in [cold, warm]:               # only meaningful for omnifs/skill
        run = invoke_agent(task.prompt, mode, cache)
        record(run.metrics, run.transcript)
```

`N` defaults to 5 trials per cell to capture variance from sampling
temperature and tool-call retries. The agent runs at a fixed
temperature (probably 0 for grading reliability, then a separate sweep
at 0.7 to capture exploration-mode behavior).

Cold vs warm:
- **cold**: omnifs cache flushed before the run; the `builtin` mode
  is implicitly cold because `gh`/`curl` are stateless; `skill` mode
  starts each run in a fresh skill process where possible.
- **warm**: omnifs has already serviced one full pass over the target
  artifact in a prior throwaway run; this is the steady-state number.

## Harness

A new top-level `evals/` workspace:

```
evals/
  corpus/
    github.json
    arxiv.json
    snapshot/
      github/{owner}/{repo}/issues/{n}.json
      github/{owner}/{repo}/pulls/{n}.json
      arxiv/{paper}.json
  runner/                      # rust binary
    src/
      main.rs                  # CLI: `evals run --mode omnifs --tier 1`
      driver.rs                # spawns the agent SDK loop
      tool_gate.rs             # allow/deny matcher
      metrics.rs               # token + timing accounting
      grader.rs                # exact/substring/LLM-judge
  results/                     # gitignored; per-run JSON + summary
  reports/
    pareto.py                  # plots tokens vs wall-clock per mode
    table.py                   # markdown summary table
```

The runner uses the Claude Agent SDK programmatically. It does not
shell out to `claude` — it owns the loop so it can enforce the tool
gate and read `usage` deltas turn by turn. The mount lifecycle (start
omnifs, wait for mount, fall through to the run, tear down) is driven
by the runner so a single `evals run` reproduces an entire cell.

## Reporting

Two artifacts per `evals run`:

1. **Per-task table** (markdown, in `results/{run-id}/table.md`):

   | task | mode | trial | cache | tokens-in | tokens-out | wall-s | calls | pass |
   |------|------|-------|-------|-----------|------------|--------|-------|------|
   | gh-tier1-001 | omnifs | 1 | cold | 1240 | 87 | 3.1 | 2 | yes |
   | gh-tier1-001 | builtin | 1 | – | 3890 | 92 | 4.8 | 3 | yes |
   | gh-tier1-001 | skill | 1 | cold | 2050 | 88 | 2.4 | 1 | yes |
   | ... |

2. **Aggregated rollup** (`results/{run-id}/summary.md`): per-mode and
   per-tier median tokens, median wall-clock, success rate, with the
   IQR alongside the median so a single weird trial doesn't carry the
   narrative.

3. **Pareto plot** (`results/{run-id}/pareto.png`): tokens on one axis,
   wall-clock on the other, one point per (task, mode) cell, mode as
   color. The dominated region tells you the bigger story than any
   single number.

## What we expect to see (hypothesis, falsifiable)

These are the predictions the framework should be able to confirm or
refute:

- **Tier 1 token win for omnifs**: a `cat /mnt/gh/.../issues/123/body`
  returns the exact bytes, no JSON envelope. `gh issue view 123 --json`
  returns the body inside a JSON object with a dozen sibling fields the
  agent has to skim past. We expect omnifs to use 30–60% fewer input
  tokens on tier-1 reads, modulo path discovery overhead on cold runs.
- **Tier 3 wall-clock win for skill**: exploration tasks reward bundled
  operations. A `find-prs-touching` skill that ships back a curated
  list beats both an agent grep'ing through omnifs and an agent
  chaining `gh search`s.
- **Cold-cache penalty for omnifs**: the first read of a deep arXiv
  path may be slower than a direct `WebFetch` because the host has to
  fetch and then project. The warm-cache run should erase the gap and
  then some.
- **No mode wins everything.** If one mode wins every cell, either the
  corpus is too narrow, the gate is leaky, or the grader is wrong.
  Confirming a pareto frontier (each mode wins some cells) is itself
  a result.

## Controls and confounders

- **Memorization.** Tasks with answers the model can plausibly recite
  from training (popular issues, famous papers) are excluded. The
  corpus prefers facts that are not on the open web in plain prose:
  exact comment counts, file diff sizes, byte ranges, byte hashes.
- **Tokenizer parity.** Model is fixed; only the tool surface varies.
  The same response payload tokenizes the same way regardless of
  which tool delivered it. Token differences come from the *shape*
  of the payload (raw bytes vs JSON envelope vs rendered markdown),
  not the encoder.
- **Cache leakage.** Cold runs in omnifs/skill mode must start from a
  flushed state. The runner's setup step asserts cache emptiness
  before kicking off the agent.
- **Skill iteration.** Skill mode benefits from skill-author tuning.
  We freeze the skill version per run and bump it deliberately, so
  re-runs are comparable but skill evolution is visible.
- **Provider iteration.** Same treatment for omnifs: the host and
  provider commits are pinned per run, recorded in the run manifest.
- **Trial count.** 5 trials per cell at temperature 0 catches
  tool-call nondeterminism; we widen to 20 for any cell where the
  IQR exceeds the median to confirm variance isn't drowning the
  signal.

## Out of scope (for v1)

- Mutations (`#[mutate]` handlers, `gh issue create`, etc.). Read-only
  to start; write benchmarks need a sandboxed fork and are a much
  bigger harness problem.
- Multi-provider tasks ("find arXiv papers cited in this GitHub
  issue"). Useful, but the cross-provider story belongs in v2 once
  the single-service numbers are credible.
- Long-horizon agentic tasks ("fix this bug"). Those are eval'd
  elsewhere (SWE-Bench-style). This framework is about information
  access cost, not end-to-end engineering.

## Open questions

- **Grader fairness for free-form answers.** The LLM judge can favor
  the mode that happens to phrase things like the judge does. We
  should probably score quote tasks with exact-match and reserve the
  judge for summary tasks, where we also report inter-judge
  agreement across two judge models.
- **Should we include a "naive web" mode** (just `WebFetch` to
  github.com HTML pages)? It's a useful low bar but it inflates the
  matrix. Probably yes for tier 1 only.
- **Reporting cost in $ vs tokens.** Cost is the number stakeholders
  care about, but cost depends on the model price; we report both
  and let the reader pick.

## Next steps

1. Stand up `evals/runner/` as an empty rust crate that boots the SDK
   and runs one hard-coded task. Confirm the token/usage accounting.
2. Wire the tool gate. Confirm `omnifs` mode genuinely can't reach
   `api.github.com`.
3. Author 10 tier-1 GitHub tasks and 5 tier-1 arXiv tasks; freeze
   their snapshots into `corpus/snapshot/`.
4. Run the first matrix; publish the table. Iterate on the corpus
   from there.
