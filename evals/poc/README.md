# PoC: token-efficiency eval

The smallest runnable version of the eval framework proposed in
`docs/design/eval-token-efficiency.md`. One task, two modes, one
metric table.

## What it does

Runs the same prompt twice through `claude -p`:

- **omnifs mode** — agent's working dir is `fixtures/omnifs/`, which
  mimics what an omnifs mount projects: `issues/1/title` is plain
  text holding only the title, `issues/1/body` holds only the body.
- **builtin mode** — agent's working dir is `fixtures/api/`, holding
  the full GitHub REST envelope (`issue_1.json`) — the same payload
  `gh api /repos/.../issues/1` would return.

Same model, same task, same allowed tool (`Read` only), same system
prompt structure (oriented to each mode's data source — the analog of
a CLAUDE.md telling the agent where omnifs mounts or how to use `gh`).
The only variable is the **shape of the bytes** the agent has to ingest.

This isolates the design doc's central hypothesis: that projected-file
payloads are cheaper in tokens than API envelopes.

This PoC deliberately skips:

- a live omnifs mount (replaced by a fixture that mimics its output)
- a live GitHub API call (replaced by a captured envelope)
- the third "skill" mode
- multiple tasks, tiers, cold/warm splits, statistical variance budgets

All of those are real features the full framework needs; none of them
are needed to produce a first number.

## Run

```
cd evals/poc
python3 run.py
```

Needs the `claude` CLI on `PATH` and a working `claude` login (or
`ANTHROPIC_API_KEY`). Defaults to `claude-haiku-4-5-20251001` and 3
trials per mode. Edit constants at the top of `run.py` to change.

## Output

```
task: What is the title of GitHub issue #1 in raulk/omnifs?
model: claude-haiku-4-5-20251001   trials per mode: 3

mode     tr     in      cc      cr  tot_in   out  wall_s turn den        $  pass
---------------------------------------------------------------------------------
omnifs   1      66   24379  163497  187942  1058   17.68   8    2   0.0527  OK
omnifs   2     ...
builtin  1     ...
...

medians per mode:
mode      tot_in   out  wall_s        $  pass
----------------------------------------------
omnifs    ...      ...    ...      ...   3/3
builtin   ...      ...    ...      ...   3/3

omnifs vs builtin: input -XX%, wall -YY%
```

Columns:

- `in` — uncached input tokens this turn
- `cc` — bytes written to prompt cache (mostly system-prompt scaffolding,
  comparable across modes)
- `cr` — bytes re-read from prompt cache across the agent loop (grows
  with turn count)
- `tot_in` — `in + cc + cr`, the gross input volume the model billed against
- `out` — assistant output tokens (all turns, including tool calls)
- `turn` — agent loop turns
- `den` — tool-use denials (the agent reached for something the mode
  gate blocked; high count = mode is unergonomic for this task)

The interesting columns are `tot_in` and `wall_s`. Payload shape drives
input volume; turn count drives wall clock.

## Next steps (not in this PoC)

- Add real tasks from each tier in the corpus.
- Add the third mode (skill) once a `github-read` skill exists.
- Replace fixtures with a real omnifs mount and a real `gh` call.
- Add cold/warm cache runs and a proper variance budget.
- Emit the markdown table + pareto plot the design doc describes.
