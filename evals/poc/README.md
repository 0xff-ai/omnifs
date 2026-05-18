# PoC: token-efficiency eval

The smallest runnable version of the framework from
`docs/design/eval-token-efficiency.md`. One task, four task cells
(2×2), plus two baseline cells that capture pure scaffolding cost
so it can be subtracted out and the marginal task cost reported.

## What it compares

Two axes on the same task ("title of issue #1"):

|                  | **fixture: omnifs**     | **fixture: api**         |
|------------------|-------------------------|--------------------------|
| **bare** install | `omnifs.bare`           | `api.bare`               |
| **full** install | `omnifs.full`           | `api.full`               |

Plus a baseline per install:

- `baseline.bare` and `baseline.full` run a no-op task ("reply with
  'ok', don't use any tools"). Their median `total_input` is the
  scaffolding cost for that install — what you pay before doing any
  real work.

### Axis 1 — payload shape

- **omnifs** fixture: `fixtures/omnifs/issues/1/title` is plain text
  holding only the title. The shape an omnifs FUSE mount projects.
- **api** fixture: `fixtures/api/issue_1.json` is the full GitHub
  REST envelope — the same payload `gh api /repos/.../issues/1`
  returns.

### Axis 2 — install

- **bare**: `--system-prompt "You answer concisely."` (replaces the
  default ~25k-token system prompt), `--tools Read` (Read only).
- **full**: no `--system-prompt` override (full default Claude Code
  system prompt), no `--tools` restriction (default tool surface:
  Read, Bash, Glob, Grep, ...). What a normal `claude` invocation
  pays on every turn. `WebFetch` / `WebSearch` denied to keep the
  comparison offline-deterministic.

Both modes use `--setting-sources ""` so host-specific user/project
settings (hooks, CLAUDE.md, plugins) don't pollute the measurement.
A real local install will be *more* expensive than `full` reports
here, since users layer their own settings on top.

## Why baselines

In `full` mode, ~96% of the input tokens per turn is the default
system prompt being re-read from prompt cache. The actual payload
the agent reads to do the task is a small fraction. Without
subtracting the baseline, the payload-shape effect is invisible —
swamped by ~25k tokens of scaffolding × every turn.

The `marginal` column shows `total_input − baseline_of_this_install`,
which is what the agent actually spent on the task work itself.

## Run

```
cd evals/poc
python3 run.py
```

Needs the `claude` CLI on `PATH` and a working `claude` login (or
`ANTHROPIC_API_KEY`). Defaults to `claude-haiku-4-5-20251001` and
3 trials per cell (18 runs total). Edit constants at the top of
`run.py`.

## Example output

```
baselines (scaffolding cost):  bare=1492 tokens, full=24487 tokens

medians per cell (marginal = tot_in − baseline-of-this-install):
cell            tot_in  marginal  mwall_s   out        $  pass
--------------------------------------------------------------
baseline.bare     1492         0     0.00    58   0.0023  3/3
baseline.full    24487         0     0.00    58   0.0033  3/3
omnifs.bare       5713      4221     6.10   655   0.0096  3/3
omnifs.full      49274     24787     1.84   268   0.0072  3/3
api.bare          6642      5150     3.72   372   0.0091  3/3
api.full         50723     26236     2.03   259   0.0089  3/3

deltas (marginal — scaffolding offset removed):
  omnifs vs api  (bare)     marginal_in  -18%   marginal_wall  +64%
  omnifs vs api  (full)     marginal_in   -6%   marginal_wall   -9%
```

Three things this output tells you:

1. **Scaffolding dominates.** The `full` install costs ~16× more
   per turn than `bare` (24487 vs 1492 baseline tokens). Once the
   agent's loop runs through 2-3 turns, that ~25k is paid 2-3 times.
2. **Payload shape is real but small.** After subtracting the
   scaffolding, omnifs costs 6–18% fewer tokens than the API
   envelope for the same answer. The win exists at both installs;
   it just gets diluted at scale.
3. **Per-turn cache re-reads are the largest line item.** Each
   additional agent turn in `full` mode adds ~22k tokens of cached
   re-read. Anything that lets the agent finish in one fewer turn
   beats anything that shaves payload bytes.

## Caveats

- 3 trials per cell is a PoC budget; the bare cells especially show
  variance because with only a tiny system prompt the model's
  tool-use pattern is less deterministic (sometimes one Read,
  sometimes two).
- Real-user installs pay extra for their own CLAUDE.md, hooks, and
  plugins on top of `full`. This is a floor.
- Fixtures are stand-ins for a real omnifs mount and a real `gh`
  call. Replace them with live sources to get production numbers.

## Next steps (not in this PoC)

- Add a third install column (skill) once a `github-read` skill exists.
- Add tier-2 and tier-3 tasks (multi-fetch, exploration).
- Replace fixtures with a real omnifs mount and a real `gh` call.
- Add cold/warm cache runs and a proper variance budget.
- Emit the markdown table + pareto plot the design doc describes.
