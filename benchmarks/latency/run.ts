#!/usr/bin/env bun

// Warm-latency measurement suite for the omnifs projected tree.
//
// Times real toolbox commands (`ls`, `cat`, `grep -r`) against a mounted omnifs
// directory and reports warm p50/p95 plus a single cold first-touch number, at
// concurrency levels 1/4/8. Each scenario is a real spawned process timed with
// `performance.now()`; there are no shell pipelines, so the number is the wall
// time a user pays to invoke the tool.
//
// Timing fidelity: run this WHERE the target path is local. For the Docker dev
// runtime the mount lives at `/omnifs` inside the container, so this script must
// run inside the container (see README) rather than driving `docker exec`
// per-op, whose ~hundreds-of-ms startup would swamp millisecond filesystem ops.

import { spawnSync } from "node:child_process";
import { readFileSync, readdirSync, statSync } from "node:fs";
import { basename, join } from "node:path";

interface Options {
  target: string;
  concurrencies: number[];
  iterations: number;
  out: string;
  warmup: number;
  subdir: string | null;
  file: string | null;
  grepLiteral: string | null;
  gitSha: string | null;
}

interface ScenarioRow {
  name: string;
  concurrency: number;
  warm: { p50_ms: number; p95_ms: number; n: number };
  cold_first_ms: number | null;
}

const ALLOWED_CONCURRENCY = new Set([1, 4, 8]);

main().catch((error) => {
  console.error(`error: ${error instanceof Error ? error.message : error}`);
  process.exit(1);
});

async function main() {
  const options = parseArgs(Bun.argv.slice(2));

  const target = options.target;
  requireDir(target, "--target");

  // Resolve the three discovered inputs. When the operator passes explicit
  // overrides no tree bytes are read before timing, which is what makes the
  // cold numbers a true first touch (see README, Cold protocol).
  const subdir = options.subdir
    ? absoluteUnder(target, options.subdir)
    : firstSubdir(target);
  requireDir(subdir, "subdir");

  const file = options.file
    ? absoluteUnder(target, options.file)
    : firstFile(subdir);
  requireFile(file, "file");

  const grepLiteral = options.grepLiteral ?? deriveLiteral(file);

  const discovered = {
    subdir,
    file,
    grep_literal: grepLiteral,
    from_overrides: Boolean(
      options.subdir && options.file && options.grepLiteral,
    ),
  };

  // Scenario order matters: the first timed spawn of each is its cold sample.
  const scenarios: Scenario[] = [
    { name: "ls", argv: () => ["ls", target], okCodes: [0] },
    { name: "ls_subdir", argv: () => ["ls", subdir], okCodes: [0] },
    { name: "cat", argv: () => ["cat", file], okCodes: [0] },
    // grep exits 1 on "no match"; the literal is present, but tolerate 1 so a
    // sampled-token miss on binary content does not abort the whole run.
    { name: "grep_r", argv: () => ["grep", "-r", grepLiteral, subdir], okCodes: [0, 1] },
  ];

  console.error(
    `target=${target}\n  subdir=${subdir}\n  file=${file}\n  grep_literal=${grepLiteral}\n  concurrency=${options.concurrencies.join(",")} iterations=${options.iterations}`,
  );

  const coldConcurrency = options.concurrencies[0];
  const rows: ScenarioRow[] = [];

  for (const scenario of scenarios) {
    // Cold: the very first touch of this scenario's path this run.
    const coldMs = await timeSpawn(scenario.argv(), scenario.okCodes);

    // Warm-up: an untimed run to guarantee caches are hot before the samples.
    for (let w = 0; w < options.warmup; w += 1) {
      await runSpawn(scenario.argv(), scenario.okCodes);
    }

    for (const concurrency of options.concurrencies) {
      const samples: number[] = [];
      for (let i = 0; i < options.iterations; i += 1) {
        const batch = await Promise.all(
          Array.from({ length: concurrency }, () =>
            timeSpawn(scenario.argv(), scenario.okCodes),
          ),
        );
        samples.push(...batch);
      }
      rows.push({
        name: scenario.name,
        concurrency,
        warm: {
          p50_ms: percentile(samples, 50),
          p95_ms: percentile(samples, 95),
          n: samples.length,
        },
        cold_first_ms: concurrency === coldConcurrency ? round(coldMs) : null,
      });
    }
  }

  const report = {
    date: new Date().toISOString().slice(0, 10),
    target,
    git_sha: resolveGitSha(options.gitSha),
    host: hostLabel(),
    iterations: options.iterations,
    concurrencies: options.concurrencies,
    discovery: discovered,
    scenarios: rows,
  };

  const json = JSON.stringify(report, null, 2);
  await Bun.write(options.out, `${json}\n`);

  const mdPath = mdSibling(options.out);
  await Bun.write(mdPath, renderMarkdown(report));

  console.error(`wrote ${options.out}`);
  console.error(`wrote ${mdPath}`);
}

interface Scenario {
  name: string;
  argv: () => string[];
  okCodes: number[];
}

// Spawn, await exit, return elapsed ms. Times a real process, no shell.
async function timeSpawn(argv: string[], okCodes: number[]): Promise<number> {
  const start = performance.now();
  const proc = Bun.spawn(argv, { stdout: "ignore", stderr: "ignore" });
  const code = await proc.exited;
  const elapsed = performance.now() - start;
  if (!okCodes.includes(code)) {
    throw new Error(`command failed (exit ${code}): ${argv.join(" ")}`);
  }
  return elapsed;
}

async function runSpawn(argv: string[], okCodes: number[]): Promise<void> {
  const proc = Bun.spawn(argv, { stdout: "ignore", stderr: "ignore" });
  const code = await proc.exited;
  if (!okCodes.includes(code)) {
    throw new Error(`command failed (exit ${code}): ${argv.join(" ")}`);
  }
}

// Nearest-rank percentile over an unsorted sample set.
function percentile(values: number[], p: number): number {
  if (values.length === 0) return NaN;
  const sorted = [...values].sort((a, b) => a - b);
  const rank = Math.ceil((p / 100) * sorted.length);
  const idx = Math.min(Math.max(rank - 1, 0), sorted.length - 1);
  return round(sorted[idx]);
}

function round(ms: number): number {
  return Math.round(ms * 1000) / 1000;
}

function firstSubdir(target: string): string {
  const entries = readdirSync(target, { withFileTypes: true }).sort((a, b) =>
    a.name.localeCompare(b.name),
  );
  for (const entry of entries) {
    if (entry.name.startsWith(".")) continue;
    const path = join(target, entry.name);
    if (isDir(path)) return path;
  }
  throw new Error(
    `no subdirectory found under ${target}; pass --subdir explicitly`,
  );
}

function firstFile(dir: string, maxDepth = 4): string {
  const found = walkForFile(dir, maxDepth);
  if (!found) {
    throw new Error(`no regular file found under ${dir}; pass --file explicitly`);
  }
  return found;
}

function walkForFile(dir: string, depth: number): string | null {
  let entries;
  try {
    entries = readdirSync(dir, { withFileTypes: true }).sort((a, b) =>
      a.name.localeCompare(b.name),
    );
  } catch {
    return null;
  }
  const dirs: string[] = [];
  for (const entry of entries) {
    if (entry.name.startsWith(".") || entry.name.startsWith("@")) continue;
    const path = join(dir, entry.name);
    if (isFile(path)) return path;
    if (depth > 0 && isDir(path)) dirs.push(path);
  }
  for (const sub of dirs) {
    const found = walkForFile(sub, depth - 1);
    if (found) return found;
  }
  return null;
}

// Sample a present literal from the target file so grep does real work.
function deriveLiteral(file: string): string {
  try {
    const buf = readFileSync(file);
    const text = buf.subarray(0, 65536).toString("utf8");
    const match = text.match(/[A-Za-z]{4,}/);
    if (match) return match[0];
  } catch {
    // fall through to the common-word default
  }
  return "the";
}

function isDir(path: string): boolean {
  try {
    return statSync(path).isDirectory();
  } catch {
    return false;
  }
}

function isFile(path: string): boolean {
  try {
    return statSync(path).isFile();
  } catch {
    return false;
  }
}

function requireDir(path: string, label: string): void {
  if (!isDir(path)) throw new Error(`${label} is not a directory: ${path}`);
}

function requireFile(path: string, label: string): void {
  if (!isFile(path)) throw new Error(`${label} is not a file: ${path}`);
}

function absoluteUnder(target: string, value: string): string {
  return value.startsWith("/") ? value : join(target, value);
}

function resolveGitSha(flag: string | null): string {
  if (flag) return flag;
  const fromEnv = process.env.OMNIFS_GIT_SHA;
  if (fromEnv) return fromEnv;
  const result = spawnSync("git", ["rev-parse", "HEAD"], { encoding: "utf8" });
  if (result.status === 0 && result.stdout) return result.stdout.trim();
  return "unknown";
}

function hostLabel(): string {
  const uname = spawnSync("uname", ["-sm"], { encoding: "utf8" });
  const os = uname.status === 0 ? uname.stdout.trim() : "unknown";
  const inContainer = isFile("/.dockerenv") ? " (container)" : "";
  return `${os}${inContainer}`;
}

function mdSibling(outPath: string): string {
  const stripped = outPath.replace(/\.json$/i, "");
  return `${stripped}.md`;
}

function renderMarkdown(report: {
  date: string;
  target: string;
  git_sha: string;
  host: string;
  iterations: number;
  concurrencies: number[];
  discovery: { subdir: string; file: string; grep_literal: string; from_overrides: boolean };
  scenarios: ScenarioRow[];
}): string {
  const lines: string[] = [];
  lines.push(`# omnifs latency report ${report.date}`);
  lines.push("");
  lines.push(`- target: \`${report.target}\``);
  lines.push(`- git sha: \`${report.git_sha}\``);
  lines.push(`- host: ${report.host}`);
  lines.push(`- iterations per (scenario, concurrency): ${report.iterations}`);
  lines.push(`- subdir: \`${report.discovery.subdir}\``);
  lines.push(`- file: \`${report.discovery.file}\``);
  lines.push(`- grep literal: \`${report.discovery.grep_literal}\``);
  lines.push(
    `- cold protocol: ${report.discovery.from_overrides ? "explicit overrides (no discovery read before timing) — cold is a true first touch" : "auto-discovery (tree read before timing) — cold is post-discovery, treat as approximate"}`,
  );
  lines.push("");
  lines.push(
    "Warm-latency target (recorded, not enforced): p95 <= 50 ms at concurrency 8.",
  );
  lines.push("");
  lines.push(
    "| scenario | concurrency | warm p50 (ms) | warm p95 (ms) | n | cold first (ms) | warm-latency target |",
  );
  lines.push("|---|---|---|---|---|---|---|");
  for (const row of report.scenarios) {
    const cold = row.cold_first_ms === null ? "" : row.cold_first_ms.toFixed(3);
    const targetResult =
      row.concurrency === 8
        ? row.warm.p95_ms <= 50
          ? "within"
          : "over"
        : "";
    lines.push(
      `| ${row.name} | ${row.concurrency} | ${row.warm.p50_ms.toFixed(3)} | ${row.warm.p95_ms.toFixed(3)} | ${row.warm.n} | ${cold} | ${targetResult} |`,
    );
  }
  lines.push("");
  return `${lines.join("\n")}\n`;
}

function parseArgs(args: string[]): Options {
  const options: Options = {
    target: "",
    concurrencies: [1, 4, 8],
    iterations: 50,
    out: "",
    warmup: 1,
    subdir: null,
    file: null,
    grepLiteral: null,
    gitSha: null,
  };
  for (let i = 0; i < args.length; i += 1) {
    const arg = args[i];
    if (arg === "--target") {
      options.target = requireValue(args, ++i, arg);
    } else if (arg === "--concurrency") {
      options.concurrencies = parseConcurrency(requireValue(args, ++i, arg));
    } else if (arg === "--iterations") {
      options.iterations = parsePositiveInt(requireValue(args, ++i, arg), arg);
    } else if (arg === "--out") {
      options.out = requireValue(args, ++i, arg);
    } else if (arg === "--warmup") {
      options.warmup = parsePositiveInt(requireValue(args, ++i, arg), arg, 0);
    } else if (arg === "--subdir") {
      options.subdir = requireValue(args, ++i, arg);
    } else if (arg === "--file") {
      options.file = requireValue(args, ++i, arg);
    } else if (arg === "--grep-literal") {
      options.grepLiteral = requireValue(args, ++i, arg);
    } else if (arg === "--git-sha") {
      options.gitSha = requireValue(args, ++i, arg);
    } else if (arg === "--help" || arg === "-h") {
      printHelp();
      process.exit(0);
    } else {
      throw new Error(`unknown argument ${arg}`);
    }
  }
  if (!options.target) throw new Error("--target is required");
  if (!options.out) throw new Error("--out is required");
  return options;
}

function parseConcurrency(value: string): number[] {
  const parts = value
    .split(",")
    .map((part) => part.trim())
    .filter((part) => part.length > 0)
    .map((part) => parsePositiveInt(part, "--concurrency"));
  if (parts.length === 0) throw new Error("--concurrency needs at least one value");
  for (const part of parts) {
    if (!ALLOWED_CONCURRENCY.has(part)) {
      throw new Error(`--concurrency values must be one of 1,4,8 (got ${part})`);
    }
  }
  return [...new Set(parts)].sort((a, b) => a - b);
}

function parsePositiveInt(value: string, flag: string, min = 1): number {
  const parsed = Number(value);
  if (!Number.isInteger(parsed) || parsed < min) {
    throw new Error(`${flag} expects an integer >= ${min} (got ${value})`);
  }
  return parsed;
}

function requireValue(args: string[], index: number, flag: string): string {
  const value = args[index];
  if (value === undefined || value.startsWith("--")) {
    throw new Error(`${flag} requires a value`);
  }
  return value;
}

function printHelp(): void {
  const name = basename(Bun.argv[1] ?? "run.ts");
  console.log(
    [
      `usage: bun ${name} --target <path> --out <file.json> [options]`,
      "",
      "options:",
      "  --target <path>        mounted omnifs directory (required)",
      "  --out <file.json>      JSON report path; a .md table is written beside it (required)",
      "  --concurrency <list>   comma list from {1,4,8} (default 1,4,8)",
      "  --iterations <N>       timed iterations per (scenario, concurrency) (default 50)",
      "  --warmup <N>           untimed warm-up runs per scenario (default 1)",
      "  --subdir <path>        override the discovered subdir (abs or relative to target)",
      "  --file <path>          override the discovered file to cat",
      "  --grep-literal <s>     override the sampled grep literal",
      "  --git-sha <sha>        record this sha (else OMNIFS_GIT_SHA env, else `git rev-parse`)",
      "",
      "For a true cold first-touch number, pass --subdir/--file/--grep-literal so no",
      "tree bytes are read before timing, and restart the runtime first (see README).",
    ].join("\n"),
  );
}
