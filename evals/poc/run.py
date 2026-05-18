#!/usr/bin/env python3
"""Smallest possible eval: run the same task across a 2x2 of cells, plus
a per-install baseline so the system-prompt overhead can be subtracted
out and the marginal task cost reported on its own.

  axis 1 — payload shape: 'omnifs' (projected files) vs 'api' (REST envelope)
  axis 2 — install: 'bare' (minimal scaffolding) vs 'full' (default Claude
           Code system prompt + default tool surface)

Baseline cells run a trivial no-op task ("reply with 'ok'") under each
install. Subtracting their total_input from each fixture cell yields
`marginal` — what the agent actually spent on the task, free of
scaffolding overhead.

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
GROUND_TRUTH = "Provider clippy regressions when WIT bindgen emits unused imports"
MODEL = "claude-haiku-4-5-20251001"
TRIALS = 3

TASK_BASE = "What is the title of GitHub issue #1 in raulk/omnifs?"

ORIENT_OMNIFS = (
    " The data is on the local filesystem at the working directory. "
    "For issue N, the title is at 'issues/N/title'. "
    "Reply with only the title text, no commentary."
)
ORIENT_API = (
    " The data is on the local filesystem at the working directory. "
    "For issue N, the GitHub REST API response is in 'issue_N.json'. "
    "Reply with only the title text, no commentary."
)

BASELINE_TASK = (
    "Reply with exactly the two-letter word 'ok' and nothing else. "
    "Do not use any tools."
)
BASELINE_TRUTH = "ok"


@dataclass
class Cell:
    name: str
    fixture: Path
    bare: bool       # bare = strip system prompt + restrict tools to Read.
                     # full = default Claude Code system prompt + tool surface.
    task: str        # complete task prompt sent as the user message
    ground_truth: str


CELLS: list[Cell] = [
    Cell("baseline.bare", FIXTURES / "omnifs", bare=True,
         task=BASELINE_TASK, ground_truth=BASELINE_TRUTH),
    Cell("baseline.full", FIXTURES / "omnifs", bare=False,
         task=BASELINE_TASK, ground_truth=BASELINE_TRUTH),
    Cell("omnifs.bare",   FIXTURES / "omnifs", bare=True,
         task=TASK_BASE + ORIENT_OMNIFS, ground_truth=GROUND_TRUTH),
    Cell("omnifs.full",   FIXTURES / "omnifs", bare=False,
         task=TASK_BASE + ORIENT_OMNIFS, ground_truth=GROUND_TRUTH),
    Cell("api.bare",      FIXTURES / "api",    bare=True,
         task=TASK_BASE + ORIENT_API, ground_truth=GROUND_TRUTH),
    Cell("api.full",      FIXTURES / "api",    bare=False,
         task=TASK_BASE + ORIENT_API, ground_truth=GROUND_TRUTH),
]


@dataclass
class Run:
    cell: str
    trial: int
    input_tokens: int
    cache_creation: int
    cache_read: int
    total_input: int
    output_tokens: int
    wall_s: float
    cost_usd: float
    turns: int
    answer: str
    denials: int
    ground_truth: str

    @property
    def passed(self) -> bool:
        return self.ground_truth.lower() in self.answer.lower()


def build_cmd(cell: Cell) -> list[str]:
    # Both modes skip user/local/project settings so host hooks and
    # CLAUDE.md don't contaminate the measurement. The bare-vs-full axis
    # varies system prompt and tool surface only.
    cmd = [
        "claude", "-p", cell.task,
        "--model", MODEL,
        "--output-format", "json",
        "--setting-sources", "",
        "--no-session-persistence",
        "--permission-mode", "default",
    ]
    if cell.bare:
        cmd += [
            "--system-prompt", "You answer concisely.",
            "--tools", "Read",
            "--allowedTools", "Read",
        ]
    else:
        # Default Claude Code install: default ~25k-token system prompt
        # and default tool surface. Pre-approve common reads so
        # non-interactive runs don't stall; block network tools so the
        # comparison stays offline-deterministic.
        cmd += [
            "--allowedTools", "Read,Bash,Glob,Grep",
            "--disallowedTools", "WebFetch,WebSearch",
        ]
    return cmd


def invoke(cell: Cell, trial: int) -> Run:
    t0 = time.monotonic()
    proc = subprocess.run(
        build_cmd(cell),
        cwd=cell.fixture,
        capture_output=True,
        text=True,
        check=False,
    )
    wall = time.monotonic() - t0
    if proc.returncode != 0:
        sys.exit(f"[{cell.name}] claude exited {proc.returncode}:\n{proc.stderr}")
    data = json.loads(proc.stdout)
    if data.get("is_error"):
        sys.exit(f"[{cell.name}] claude returned error:\n{data.get('result')}")
    u = data.get("usage", {})
    ci = u.get("input_tokens", 0)
    cc = u.get("cache_creation_input_tokens", 0)
    cr = u.get("cache_read_input_tokens", 0)
    return Run(
        cell=cell.name,
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
        ground_truth=cell.ground_truth,
    )


def median(xs):
    s = sorted(xs)
    n = len(s)
    return s[n // 2] if n % 2 else (s[n // 2 - 1] + s[n // 2]) / 2


def pct(new, base):
    return f"{(new - base) / base * 100:+.0f}%" if base else "n/a"


def main() -> None:
    print(f"task: {TASK_BASE}")
    print(f"model: {MODEL}   trials per cell: {TRIALS}   cells: {len(CELLS)}")
    print()
    hdr = (f"{'cell':<14} {'tr':<3} {'in':>5} {'cc':>7} {'cr':>7} "
           f"{'tot_in':>7} {'out':>5} {'wall_s':>7} {'turn':>4} "
           f"{'den':>3} {'$':>8}  pass")
    print(hdr)
    print("-" * len(hdr))

    runs: list[Run] = []
    for cell in CELLS:
        for t in range(1, TRIALS + 1):
            r = invoke(cell, t)
            runs.append(r)
            print(f"{r.cell:<14} {r.trial:<3} {r.input_tokens:>5} "
                  f"{r.cache_creation:>7} {r.cache_read:>7} "
                  f"{r.total_input:>7} {r.output_tokens:>5} "
                  f"{r.wall_s:>7.2f} {r.turns:>4} {r.denials:>3} "
                  f"{r.cost_usd:>8.4f}  {'OK' if r.passed else 'FAIL'}")

    # Per-install baselines: the cost of the scaffolding alone.
    base = {
        "bare": median([r.total_input for r in runs if r.cell == "baseline.bare"]),
        "full": median([r.total_input for r in runs if r.cell == "baseline.full"]),
    }
    base_wall = {
        "bare": median([r.wall_s for r in runs if r.cell == "baseline.bare"]),
        "full": median([r.wall_s for r in runs if r.cell == "baseline.full"]),
    }

    print()
    print(f"baselines (scaffolding cost):  "
          f"bare={int(base['bare'])} tokens, "
          f"full={int(base['full'])} tokens")
    print()

    print("medians per cell (marginal = tot_in − baseline-of-this-install):")
    print(f"{'cell':<14} {'tot_in':>7} {'marginal':>9} {'mwall_s':>8} "
          f"{'out':>5} {'$':>8}  pass")
    print("-" * 62)
    cell_med = {}
    for cell in CELLS:
        rs = [r for r in runs if r.cell == cell.name]
        m_in = median([r.total_input for r in rs])
        m_out = median([r.output_tokens for r in rs])
        m_wall = median([r.wall_s for r in rs])
        m_cost = median([r.cost_usd for r in rs])
        install = "bare" if cell.bare else "full"
        marginal_in = max(0, m_in - base[install])
        marginal_wall = max(0.0, m_wall - base_wall[install])
        cell_med[cell.name] = (m_in, marginal_in, m_wall, marginal_wall, m_cost)
        print(f"{cell.name:<14} {int(m_in):>7} {int(marginal_in):>9} "
              f"{marginal_wall:>8.2f} {int(m_out):>5} "
              f"{m_cost:>8.4f}  "
              f"{sum(r.passed for r in rs)}/{len(rs)}")

    print()
    print("deltas (marginal — scaffolding offset removed):")
    pairs = [
        ("omnifs vs api  (bare)", "omnifs.bare", "api.bare"),
        ("omnifs vs api  (full)", "omnifs.full", "api.full"),
    ]
    for label, a, b in pairs:
        if a in cell_med and b in cell_med:
            _, ai, _, aw, _ = cell_med[a]
            _, bi, _, bw, _ = cell_med[b]
            print(f"  {label:<24}  marginal_in {pct(ai, bi):>5}   "
                  f"marginal_wall {pct(aw, bw):>5}")

    out = ROOT / "results.json"
    out.write_text(json.dumps([asdict(r) for r in runs], indent=2))
    print(f"\nraw: {out}")


if __name__ == "__main__":
    main()
