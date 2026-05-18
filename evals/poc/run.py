#!/usr/bin/env python3
"""Smallest possible eval: run the same task across a 2x2 of cells:

  axis 1 — payload shape: 'omnifs' (projected files) vs 'api' (REST envelope)
  axis 2 — install: 'bare' (minimal scaffolding) vs 'full' (default Claude
           Code system prompt + default tool surface)

Bare cells isolate payload-shape effects from system-prompt noise. Full
cells show what those effects look like on top of the real ~25k-token
default scaffolding a normal Claude Code user pays on every turn.

Usage:  python3 run.py
"""

from __future__ import annotations

import json
import subprocess
import sys
import time
from dataclasses import dataclass, asdict, field
from pathlib import Path

ROOT = Path(__file__).parent
FIXTURES = ROOT / "fixtures"
GROUND_TRUTH = "Provider clippy regressions when WIT bindgen emits unused imports"
MODEL = "claude-haiku-4-5-20251001"
TRIALS = 3

TASK_BASE = "What is the title of GitHub issue #1 in raulk/omnifs?"

ORIENT_OMNIFS = (
    " The data is on the local filesystem at the working directory. "
    "For issue N: title is at 'issues/N/title', body at 'issues/N/body', "
    "state at 'issues/N/state'. Reply with only the title text, no commentary."
)
ORIENT_API = (
    " The data is on the local filesystem at the working directory. "
    "For issue N, the GitHub REST API response is in 'issue_N.json'. "
    "Reply with only the title text, no commentary."
)


@dataclass
class Cell:
    name: str
    fixture: Path
    bare: bool        # bare = minimal --system-prompt, --tools Read only,
                      # --setting-sources "" so default ~25k scaffolding is skipped.
                      # full = no overrides; default Claude Code surface.
    orientation: str  # appended to the user-facing task prompt


CELLS: list[Cell] = [
    Cell("omnifs.bare", FIXTURES / "omnifs", bare=True,  orientation=ORIENT_OMNIFS),
    Cell("omnifs.full", FIXTURES / "omnifs", bare=False, orientation=ORIENT_OMNIFS),
    Cell("api.bare",    FIXTURES / "api",    bare=True,  orientation=ORIENT_API),
    Cell("api.full",    FIXTURES / "api",    bare=False, orientation=ORIENT_API),
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

    @property
    def passed(self) -> bool:
        return GROUND_TRUTH.lower() in self.answer.lower()


def build_cmd(cell: Cell, prompt: str) -> list[str]:
    # Both modes skip user/local/project settings so the host
    # environment's hooks and CLAUDE.md don't contaminate the measurement.
    # The bare-vs-full axis varies system prompt and tool surface only.
    cmd = [
        "claude", "-p", prompt,
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
        # non-interactive runs don't stall on permission prompts; block
        # network tools so the comparison stays offline-deterministic.
        cmd += [
            "--allowedTools", "Read,Bash,Glob,Grep",
            "--disallowedTools", "WebFetch,WebSearch",
        ]
    return cmd


def invoke(cell: Cell, trial: int) -> Run:
    prompt = TASK_BASE + cell.orientation
    t0 = time.monotonic()
    proc = subprocess.run(
        build_cmd(cell, prompt),
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
    hdr = (f"{'cell':<12} {'tr':<3} {'in':>5} {'cc':>7} {'cr':>7} "
           f"{'tot_in':>7} {'out':>5} {'wall_s':>7} {'turn':>4} "
           f"{'den':>3} {'$':>8}  pass")
    print(hdr)
    print("-" * len(hdr))

    runs: list[Run] = []
    for cell in CELLS:
        for t in range(1, TRIALS + 1):
            r = invoke(cell, t)
            runs.append(r)
            print(f"{r.cell:<12} {r.trial:<3} {r.input_tokens:>5} "
                  f"{r.cache_creation:>7} {r.cache_read:>7} "
                  f"{r.total_input:>7} {r.output_tokens:>5} "
                  f"{r.wall_s:>7.2f} {r.turns:>4} {r.denials:>3} "
                  f"{r.cost_usd:>8.4f}  {'OK' if r.passed else 'FAIL'}")

    print()
    print("medians per cell:")
    print(f"{'cell':<12} {'tot_in':>7} {'out':>5} {'wall_s':>7} {'$':>8}  pass")
    print("-" * 50)
    cell_med = {}
    for cell in CELLS:
        rs = [r for r in runs if r.cell == cell.name]
        m_in = median([r.total_input for r in rs])
        m_out = median([r.output_tokens for r in rs])
        m_wall = median([r.wall_s for r in rs])
        m_cost = median([r.cost_usd for r in rs])
        cell_med[cell.name] = (m_in, m_wall, m_cost)
        print(f"{cell.name:<12} {int(m_in):>7} {int(m_out):>5} "
              f"{m_wall:>7.2f} {m_cost:>8.4f}  "
              f"{sum(r.passed for r in rs)}/{len(rs)}")

    print()
    print("deltas:")
    pairs = [
        ("omnifs vs api  (bare)", "omnifs.bare", "api.bare"),
        ("omnifs vs api  (full)", "omnifs.full", "api.full"),
        ("bare vs full  (omnifs)", "omnifs.bare", "omnifs.full"),
        ("bare vs full  (api)",    "api.bare",    "api.full"),
    ]
    for label, a, b in pairs:
        if a in cell_med and b in cell_med:
            ai, aw, _ = cell_med[a]
            bi, bw, _ = cell_med[b]
            print(f"  {label:<24}  tot_in {pct(ai, bi):>5}   "
                  f"wall {pct(aw, bw):>5}")

    out = ROOT / "results.json"
    out.write_text(json.dumps([asdict(r) for r in runs], indent=2))
    print(f"\nraw: {out}")


if __name__ == "__main__":
    main()
