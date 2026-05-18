#!/usr/bin/env python3
"""Smallest possible eval: run the same task twice through `claude -p`,
once in 'omnifs' mode (fixture is plain projected files) and once in
'builtin' mode (fixture is the GitHub API JSON envelope). Print a table.

Same model, same task, same tool (Read). Only the payload shape varies
— and each mode's system prompt orients the agent to its own data
source (the equivalent of CLAUDE.md telling the agent where omnifs
mounts, or how to use `gh`).

Usage:  python3 run.py
"""

from __future__ import annotations

import json
import subprocess
import sys
import time
from dataclasses import dataclass, asdict
from pathlib import Path

ROOT = Path(__file__).parent
FIXTURES = ROOT / "fixtures"
TASK = "What is the title of GitHub issue #1 in raulk/omnifs?"
GROUND_TRUTH = "Provider clippy regressions when WIT bindgen emits unused imports"
MODEL = "claude-haiku-4-5-20251001"
TRIALS = 3

MODES = {
    "omnifs": {
        "dir": FIXTURES / "omnifs",
        "system_prompt": (
            "You answer questions by reading local files. The user's "
            "GitHub data is projected onto the filesystem at the working "
            "directory. For issue N, the title is in 'issues/N/title', "
            "body in 'issues/N/body', and state in 'issues/N/state'. "
            "Reply with only the requested value, no commentary."
        ),
    },
    "builtin": {
        "dir": FIXTURES / "api",
        "system_prompt": (
            "You answer questions by reading local files. The user's "
            "working directory contains GitHub REST API responses. For "
            "issue N, the response is in 'issue_N.json' — the same JSON "
            "envelope the GitHub API returns. Reply with only the "
            "requested value, no commentary."
        ),
    },
}


@dataclass
class Run:
    mode: str
    trial: int
    input_tokens: int          # uncached input on this request
    cache_creation: int        # written to prompt cache
    cache_read: int            # re-read from prompt cache (across turns)
    total_input: int           # sum of the three above
    output_tokens: int
    wall_s: float
    cost_usd: float
    turns: int
    answer: str
    denials: int               # tool-use denials (mode gate caught a reach)

    @property
    def passed(self) -> bool:
        return GROUND_TRUTH.lower() in self.answer.lower()


def invoke(mode: str, cfg: dict, trial: int) -> Run:
    t0 = time.monotonic()
    proc = subprocess.run(
        [
            "claude", "-p", TASK,
            "--system-prompt", cfg["system_prompt"],
            "--model", MODEL,
            "--output-format", "json",
            "--tools", "Read",
            "--allowedTools", "Read",
            "--setting-sources", "",
            "--no-session-persistence",
            "--permission-mode", "default",
        ],
        cwd=cfg["dir"],
        capture_output=True,
        text=True,
        check=False,
    )
    wall = time.monotonic() - t0
    if proc.returncode != 0:
        sys.exit(f"[{mode}] claude exited {proc.returncode}:\n{proc.stderr}")
    data = json.loads(proc.stdout)
    if data.get("is_error"):
        sys.exit(f"[{mode}] claude returned error:\n{data.get('result')}")
    u = data.get("usage", {})
    ci = u.get("input_tokens", 0)
    cc = u.get("cache_creation_input_tokens", 0)
    cr = u.get("cache_read_input_tokens", 0)
    return Run(
        mode=mode,
        trial=trial,
        input_tokens=ci,
        cache_creation=cc,
        cache_read=cr,
        total_input=ci + cc + cr,
        output_tokens=u.get("output_tokens", 0),
        wall_s=wall,
        cost_usd=data.get("total_cost_usd", 0.0),
        turns=data.get("num_turns", 0),
        answer=(data.get("result") or "").strip(),
        denials=len(data.get("permission_denials") or []),
    )


def median(xs):
    s = sorted(xs)
    n = len(s)
    return s[n // 2] if n % 2 else (s[n // 2 - 1] + s[n // 2]) / 2


def main() -> None:
    print(f"task: {TASK}")
    print(f"model: {MODEL}   trials per mode: {TRIALS}")
    print()
    hdr = (f"{'mode':<8} {'tr':<3} {'in':>5} {'cc':>7} {'cr':>7} "
           f"{'tot_in':>7} {'out':>5} {'wall_s':>7} {'turn':>4} "
           f"{'den':>3} {'$':>8}  pass")
    print(hdr)
    print("-" * len(hdr))

    runs: list[Run] = []
    for mode, cfg in MODES.items():
        for t in range(1, TRIALS + 1):
            r = invoke(mode, cfg, t)
            runs.append(r)
            print(f"{r.mode:<8} {r.trial:<3} {r.input_tokens:>5} "
                  f"{r.cache_creation:>7} {r.cache_read:>7} "
                  f"{r.total_input:>7} {r.output_tokens:>5} "
                  f"{r.wall_s:>7.2f} {r.turns:>4} {r.denials:>3} "
                  f"{r.cost_usd:>8.4f}  {'OK' if r.passed else 'FAIL'}")

    print()
    print("medians per mode:")
    print(f"{'mode':<8} {'tot_in':>7} {'out':>5} {'wall_s':>7} {'$':>8}  pass")
    print("-" * 46)
    for mode in MODES:
        rs = [r for r in runs if r.mode == mode]
        print(f"{mode:<8} "
              f"{int(median([r.total_input for r in rs])):>7} "
              f"{int(median([r.output_tokens for r in rs])):>5} "
              f"{median([r.wall_s for r in rs]):>7.2f} "
              f"{median([r.cost_usd for r in rs]):>8.4f}  "
              f"{sum(r.passed for r in rs)}/{len(rs)}")

    # Delta: what does choosing omnifs save you?
    o_in  = median([r.total_input for r in runs if r.mode == "omnifs"])
    b_in  = median([r.total_input for r in runs if r.mode == "builtin"])
    o_w   = median([r.wall_s      for r in runs if r.mode == "omnifs"])
    b_w   = median([r.wall_s      for r in runs if r.mode == "builtin"])
    if b_in:
        print(f"\nomnifs vs builtin: input {(o_in-b_in)/b_in*100:+.0f}%, "
              f"wall {(o_w-b_w)/b_w*100:+.0f}%")

    out = ROOT / "results.json"
    out.write_text(json.dumps([asdict(r) for r in runs], indent=2))
    print(f"\nraw: {out}")


if __name__ == "__main__":
    main()
