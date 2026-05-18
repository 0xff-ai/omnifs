# PoC: token-efficiency eval

The smallest runnable version of the framework from
`docs/design/eval-token-efficiency.md`. One task, **four cells**
(2×2), one metric table.

## What it compares

A 2×2 of conditions on the same task ("title of issue #1"):

|                | **fixture: omnifs**       | **fixture: api**          |
|----------------|---------------------------|---------------------------|
| **bare** install | `omnifs.bare`           | `api.bare`                |
| **full** install | `omnifs.full`           | `api.full`                |

Axis 1 — **payload shape:**

- **omnifs** fixture: `fixtures/omnifs/issues/1/title` is plain text
  holding only the title. The shape an omnifs FUSE mount projects.
- **api** fixture: `fixtures/api/issue_1.json` is the full GitHub
  REST envelope — the same payload `gh api /repos/.../issues/1`
  returns.

Axis 2 — **install:**

- **bare**: `--system-prompt "You answer concisely."` (replaces the
  default ~25k-token system prompt), `--tools Read` (only Read
  available). Isolates the payload-shape variable from scaffolding
  cost.
- **full**: no `--system-prompt` override (full default Claude Code
  system prompt), no `--tools` restriction (default tool surface:
  Read, Bash, Glob, Grep, ...). What a normal `claude` invocation
  pays on every turn. `WebFetch`/`WebSearch` are denied to keep the
  comparison offline-deterministic.

Both modes use `--setting-sources ""` so host-specific user/project
settings (hooks, CLAUDE.md, plugins) don't pollute the measurement.
A real local install will be *more* expensive than `full` reports
here, since real users have their own settings layered on top.

Same model, same task prompt, same fixture content per axis. Only
the system prompt and the tool surface vary across the install axis;
only the bytes the `Read` tool returns vary across the payload axis.

## What the four cells tell you

- **`omnifs.bare` vs `api.bare`** — pure payload-shape effect.
  Isolates the design doc's central hypothesis with no scaffolding
  in the way.
- **`omnifs.full` vs `api.full`** — payload shape on top of the real
  Claude Code install. Shows whether the bare-mode advantage
  survives once the default scaffolding is loaded, or whether it
  gets swamped.
- **`omnifs.bare` vs `omnifs.full`** (and `api.bare` vs `api.full`)
  — the cost of the default scaffolding itself, holding payload
  shape constant.

## Run

```
cd evals/poc
python3 run.py
```

Needs the `claude` CLI on `PATH` and a working `claude` login (or
`ANTHROPIC_API_KEY`). Defaults to `claude-haiku-4-5-20251001` and
3 trials per cell (12 runs total). Edit constants at the top of
`run.py` to change.

## Output

```
task: What is the title of GitHub issue #1 in raulk/omnifs?
model: claude-haiku-4-5-20251001   trials per cell: 3   cells: 4

cell         tr     in      cc      cr  tot_in   out  wall_s turn den        $  pass
------------------------------------------------------------------------------------
omnifs.bare  1    ...
omnifs.full  1    ...    2541   46758   49317   ...
api.bare     1    ...
api.full     1    ...    3953   46741   50712   ...
...

medians per cell:
cell          tot_in   out  wall_s        $  pass
--------------------------------------------------
omnifs.bare    ...
omnifs.full    ...
api.bare       ...
api.full       ...

deltas:
  omnifs vs api  (bare)     tot_in  ...    wall  ...
  omnifs vs api  (full)     tot_in  ...    wall  ...
  bare vs full  (omnifs)    tot_in  ...    wall  ...
  bare vs full  (api)       tot_in  ...    wall  ...
```

Columns:

- `in` — uncached input tokens this turn
- `cc` — bytes written to prompt cache (large in `full` cells —
  that's the default system prompt landing in cache on first use)
- `cr` — bytes re-read from prompt cache across the agent loop
- `tot_in` — `in + cc + cr`, the gross input volume billed
- `out` — assistant output tokens across all turns
- `turn` — agent loop turns
- `den` — tool-use denials

## Caveats

- 3 trials per cell isn't enough for tight CIs; this is a PoC.
- Real-user installs pay extra for their own CLAUDE.md, hooks, and
  any plugins. The PoC's `full` mode is a floor for the real cost.
- Fixtures are stand-ins for a real omnifs mount and a real `gh`
  call. Replace them with live sources to get production numbers.

## Next steps (not in this PoC)

- Add a third install column (skill) once a `github-read` skill exists.
- Add tier-2 and tier-3 tasks (multi-fetch, exploration).
- Replace fixtures with a real omnifs mount and a real `gh` call.
- Add cold/warm cache runs and a proper variance budget.
- Emit the markdown table + pareto plot the design doc describes.
