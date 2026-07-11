#!/usr/bin/env bun
// Agentbench token-efficiency benchmark runner.
//
// Runs each task under two conditions with the same model and grades the
// outcome, measuring success, tokens, wall clock, and turn count:
//   - condition `mount`: the fixture corpus served as a filesystem (omnifs);
//     the agent runs with cwd set to the mounted root and file tools + bash.
//   - condition `mcp`:   the same bytes behind the two-tool MCP baseline; the
//     agent may use only `list_dir` and `read_file`.
//
// The model is invoked headlessly through the Claude Code CLI in print mode
// with JSON output. `--dry-run` executes everything except the model call,
// substituting a canned transcript so plumbing can be proven without cost.
//
// Probed against Claude CLI 2.1.199 (`claude --help`): there is no `--max-turns`
// flag in this version, so a task's `max_turns` is a soft cap used for grading
// (a run whose reported `num_turns` exceeds it is failed), not a CLI argument.

import { YAML } from "bun";
import {
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { grade, type Task } from "./grade.ts";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const tasksDir = join(scriptDir, "tasks");
const defaultFixtureRoot = join(scriptDir, "fixture-data");
const mcpServer = join(scriptDir, "mcp-baseline", "server.ts");
const reportsDir = join(scriptDir, "..", "reports");

type Condition = "mount" | "mcp";

interface Args {
  tasks: string[] | null; // null = all
  conditions: Condition[];
  mountRoot: string;
  fixtureRoot: string;
  model: string;
  out: string | null;
  dryRun: boolean;
  allowJudge: boolean;
  claudeBin: string;
  maxTurns: number | null;
}

function parseArgs(argv: string[]): Args {
  const a: Args = {
    tasks: null,
    conditions: ["mount", "mcp"],
    mountRoot: defaultFixtureRoot,
    fixtureRoot: defaultFixtureRoot,
    model: "opus",
    out: null,
    dryRun: false,
    allowJudge: false,
    claudeBin: "claude",
    maxTurns: null,
  };
  const list = (v: string) => v.split(/[,\s]+/).filter(Boolean);
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    const next = () => argv[++i];
    switch (arg) {
      case "--tasks":
        a.tasks = list(next());
        break;
      case "--conditions":
        a.conditions = list(next()) as Condition[];
        break;
      case "--mount-root":
        a.mountRoot = resolve(next());
        break;
      case "--fixture-root":
        a.fixtureRoot = resolve(next());
        break;
      case "--model":
        a.model = next();
        break;
      case "--out":
        a.out = resolve(next());
        break;
      case "--dry-run":
        a.dryRun = true;
        break;
      case "--allow-judge":
        a.allowJudge = true;
        break;
      case "--claude-bin":
        a.claudeBin = next();
        break;
      case "--max-turns":
        a.maxTurns = Number(next());
        break;
      case "-h":
      case "--help":
        printHelp();
        process.exit(0);
        break;
      default:
        throw new Error(`unknown argument: ${arg}`);
    }
  }
  return a;
}

function printHelp(): void {
  process.stdout.write(
    `agentbench runner\n\n` +
      `Usage: bun runner.ts [options]\n\n` +
      `  --tasks <ids>          comma/space list of task ids (default: all)\n` +
      `  --conditions <list>    subset of "mount,mcp" (default: both)\n` +
      `  --mount-root <path>    cwd for condition mount (default: fixture-data)\n` +
      `  --fixture-root <path>  dataset root for the MCP baseline (default: fixture-data)\n` +
      `  --model <id>           model passed to the Claude CLI (default: opus)\n` +
      `  --out <file>           report path (default: ../reports/agentbench-<date>.json)\n` +
      `  --dry-run              skip the model call; use a canned transcript\n` +
      `  --allow-judge          permit judge-type tasks as ungraded results\n` +
      `  --claude-bin <path>    Claude CLI binary (default: claude)\n` +
      `  --max-turns <n>        override the per-task soft turn cap\n`,
  );
}

function parseTask(text: string, file: string): Task {
  const doc = YAML.parse(text) as {
    id?: unknown;
    family?: unknown;
    prompt?: unknown;
    success?: { type?: unknown; value?: unknown };
    max_turns?: unknown;
  };
  const { id, family, prompt } = doc;
  const st = doc.success?.type;
  const sv = doc.success?.value;
  if (typeof id !== "string" || typeof family !== "string" || typeof prompt !== "string") {
    throw new Error(`task ${file}: id, family, and prompt must be strings`);
  }
  if (st !== "contains" && st !== "regex" && st !== "judge") {
    throw new Error(`task ${id}: invalid success.type "${String(st)}"`);
  }
  if (typeof sv !== "string") {
    throw new Error(`task ${id}: success.value must be a string`);
  }
  if (typeof doc.max_turns !== "number") {
    throw new Error(`task ${id}: max_turns must be a number`);
  }
  return { id, family, prompt, success: { type: st, value: sv }, max_turns: doc.max_turns };
}

function loadTasks(select: string[] | null): Task[] {
  const files = readdirSync(tasksDir).filter((f) => f.endsWith(".yaml")).sort();
  const all = files.map((f) => parseTask(readFileSync(join(tasksDir, f), "utf8"), f));
  if (!select) return all;
  const byId = new Map(all.map((t) => [t.id, t]));
  return select.map((id) => {
    const t = byId.get(id);
    if (!t) throw new Error(`unknown task id: ${id}`);
    return t;
  });
}

interface RunResult {
  answer: string;
  tokensTotal: number;
  turns: number;
  wallMs: number;
  resolvedModel: string | null;
}

// Build the exact Claude CLI argv for a task under a condition. Returns the argv
// plus the cwd and any temp files to clean up. Used for both real and dry runs
// (dry runs construct it to prove the shape, then skip execution).
function buildInvocation(
  task: Task,
  condition: Condition,
  args: Args,
): { argv: string[]; cwd: string; cleanup: () => void } {
  const common = [
    "-p",
    task.prompt,
    "--output-format",
    "json",
    "--model",
    args.model,
    "--permission-mode",
    "bypassPermissions",
  ];
  if (condition === "mount") {
    return {
      argv: [args.claudeBin, ...common, "--tools", "Read,Grep,Glob,Bash"],
      cwd: args.mountRoot,
      cleanup: () => {},
    };
  }
  // condition mcp
  const tmp = mkdtempSync(join(tmpdir(), "agentbench-mcp-"));
  const cfgPath = join(tmp, "mcp.json");
  const cfg = {
    mcpServers: {
      fixture: {
        command: "bun",
        args: [mcpServer],
        env: { FIXTURE_ROOT: args.fixtureRoot },
      },
    },
  };
  writeFileSync(cfgPath, JSON.stringify(cfg));
  return {
    argv: [
      args.claudeBin,
      ...common,
      "--mcp-config",
      cfgPath,
      "--strict-mcp-config",
      "--tools",
      "mcp__fixture__list_dir,mcp__fixture__read_file",
    ],
    cwd: tmp,
    cleanup: () => rmSync(tmp, { recursive: true, force: true }),
  };
}

// Canned transcript for --dry-run: a plausible matching answer plus placeholder
// usage that differs by condition, so the aggregation and headline math are
// exercised. Clearly marked as dry-run in the report; not real measurements.
function cannedResult(task: Task, condition: Condition): RunResult {
  const sample =
    task.success.type === "regex"
      ? task.success.value.replace(/\\b/g, "").replace(/\\/g, "")
      : task.success.value;
  const answer = `Based on the dataset, the answer is ${sample}.`;
  const usage =
    condition === "mount"
      ? { tokensTotal: 1000, turns: 2, wallMs: 400 }
      : { tokensTotal: 3300, turns: 5, wallMs: 1500 };
  return { answer, ...usage, resolvedModel: null };
}

async function runModel(
  argv: string[],
  cwd: string,
): Promise<RunResult> {
  const proc = Bun.spawn(argv, { cwd, stdout: "pipe", stderr: "pipe" });
  const stdout = await new Response(proc.stdout).text();
  const code = await proc.exited;
  if (code !== 0) {
    const stderr = await new Response(proc.stderr).text();
    throw new Error(`claude exited ${code}: ${stderr.slice(0, 500)}`);
  }
  const parsed = JSON.parse(stdout) as {
    result?: string;
    num_turns?: number;
    duration_ms?: number;
    usage?: {
      input_tokens?: number;
      output_tokens?: number;
      cache_creation_input_tokens?: number;
      cache_read_input_tokens?: number;
    };
    modelUsage?: Record<string, unknown>;
  };
  const u = parsed.usage ?? {};
  const tokensTotal =
    (u.input_tokens ?? 0) +
    (u.output_tokens ?? 0) +
    (u.cache_creation_input_tokens ?? 0) +
    (u.cache_read_input_tokens ?? 0);
  const resolvedModel = parsed.modelUsage
    ? (Object.keys(parsed.modelUsage)[0] ?? null)
    : null;
  return {
    answer: parsed.result ?? "",
    tokensTotal,
    turns: parsed.num_turns ?? 0,
    wallMs: parsed.duration_ms ?? 0,
    resolvedModel,
  };
}

interface Row {
  id: string;
  family: string;
  condition: Condition;
  success: boolean | null;
  tokens_total: number;
  turns: number;
  wall_ms: number;
}

async function main(): Promise<void> {
  const args = parseArgs(Bun.argv.slice(2));
  const tasks = loadTasks(args.tasks);

  // Refuse judge tasks before any model call unless the operator explicitly
  // accepts that their results will be ungraded.
  const judgeTasks = tasks.filter((t) => t.success.type === "judge");
  if (judgeTasks.length && !args.allowJudge) {
    process.stderr.write(
      `refusing to run judge task(s) [${judgeTasks
        .map((t) => t.id)
        .join(", ")}] without --allow-judge\n`,
    );
    process.exit(2);
  }

  const rows: Row[] = [];
  let resolvedModel: string | null = null;

  for (const task of tasks) {
    for (const condition of args.conditions) {
      const { argv, cwd, cleanup } = buildInvocation(task, condition, args);
      try {
        const run = args.dryRun
          ? cannedResult(task, condition)
          : await runModel(argv, cwd);
        if (run.resolvedModel) resolvedModel = run.resolvedModel;

        const cap = args.maxTurns ?? task.max_turns;
        let success: boolean | null;
        if (run.turns > cap) {
          success = false;
        } else {
          success = grade(task, run.answer, { allowJudge: args.allowJudge }).success;
        }
        rows.push({
          id: task.id,
          family: task.family,
          condition,
          success,
          tokens_total: run.tokensTotal,
          turns: run.turns,
          wall_ms: run.wallMs,
        });
        process.stderr.write(
          `[${args.dryRun ? "dry" : "run"}] ${task.id} / ${condition}: ` +
            `success=${success} tokens=${run.tokensTotal} turns=${run.turns}\n`,
        );
      } finally {
        cleanup();
      }
    }
  }

  const report = buildReport(args, rows, resolvedModel);
  writeReport(args, report);
}

interface CondAgg {
  n: number;
  success: number;
  success_rate: number;
  tokens_total: number;
  avg_tokens: number;
  avg_turns: number;
  avg_wall_ms: number;
}

function aggregate(rows: Row[]): CondAgg {
  const n = rows.length;
  const graded = rows.filter((r) => r.success !== null);
  const success = graded.filter((r) => r.success).length;
  const tokens = rows.reduce((s, r) => s + r.tokens_total, 0);
  return {
    n,
    success,
    success_rate: graded.length ? success / graded.length : 0,
    tokens_total: tokens,
    avg_tokens: n ? tokens / n : 0,
    avg_turns: n ? rows.reduce((s, r) => s + r.turns, 0) / n : 0,
    avg_wall_ms: n ? rows.reduce((s, r) => s + r.wall_ms, 0) / n : 0,
  };
}

function gitSha(): string {
  try {
    return require("node:child_process")
      .execSync("git rev-parse --short HEAD", { encoding: "utf8" })
      .trim();
  } catch {
    return "unknown";
  }
}

function buildReport(args: Args, rows: Row[], resolvedModel: string | null) {
  const date = new Date().toISOString().slice(0, 10);
  const families = [...new Set(rows.map((r) => r.family))].sort();
  const byFamily: Record<string, Record<string, CondAgg>> = {};
  for (const fam of families) {
    byFamily[fam] = {};
    for (const cond of args.conditions) {
      byFamily[fam][cond] = aggregate(
        rows.filter((r) => r.family === fam && r.condition === cond),
      );
    }
  }
  const overall: Record<string, CondAgg> = {};
  for (const cond of args.conditions) {
    overall[cond] = aggregate(rows.filter((r) => r.condition === cond));
  }

  const mount = overall.mount;
  const mcp = overall.mcp;
  const headline: { total_token_ratio: number | null; success_delta_pp: number | null } = {
    total_token_ratio:
      mount && mcp && mcp.tokens_total > 0
        ? mount.tokens_total / mcp.tokens_total
        : null,
    success_delta_pp:
      mount && mcp
        ? (mount.success_rate - mcp.success_rate) * 100
        : null,
  };

  return {
    date,
    requested_model: args.model,
    resolved_model: resolvedModel,
    git_sha: gitSha(),
    dry_run: args.dryRun,
    conditions: args.conditions,
    rows,
    aggregates: { by_family: byFamily, overall, headline },
  };
}

type Report = ReturnType<typeof buildReport>;

function pct(x: number): string {
  return `${(x * 100).toFixed(0)}%`;
}

function markdown(report: Report): string {
  const { aggregates } = report;
  const lines: string[] = [];
  lines.push(`# Agentbench report ${report.date}`);
  lines.push("");
  if (report.dry_run) {
    lines.push(
      "**Dry run: token, turn, and wall-clock numbers are canned placeholders, not measurements.**",
    );
    lines.push("");
  }
  lines.push(`- requested model: \`${report.requested_model}\``);
  lines.push(`- resolved model: \`${report.resolved_model ?? "n/a"}\``);
  lines.push(`- git sha: \`${report.git_sha}\``);
  lines.push(`- conditions: ${report.conditions.join(", ")}`);
  lines.push("");

  const h = aggregates.headline;
  lines.push("## Token-efficiency headline");
  lines.push("");
  lines.push(
    `- total-token ratio (mount / mcp): ${h.total_token_ratio === null ? "n/a" : h.total_token_ratio.toFixed(3)}`,
  );
  lines.push(
    `- success delta (mount - mcp): ${h.success_delta_pp === null ? "n/a" : `${h.success_delta_pp.toFixed(0)} pp`}`,
  );
  lines.push("");

  lines.push("## Per-family aggregates");
  lines.push("");
  lines.push("| family | condition | n | success | tokens | avg turns |");
  lines.push("|---|---|---|---|---|---|");
  for (const [fam, conds] of Object.entries(aggregates.by_family)) {
    for (const [cond, agg] of Object.entries(conds)) {
      lines.push(
        `| ${fam} | ${cond} | ${agg.n} | ${pct(agg.success_rate)} | ${agg.tokens_total} | ${agg.avg_turns.toFixed(1)} |`,
      );
    }
  }
  lines.push("");

  lines.push("## Per-task rows");
  lines.push("");
  lines.push("| id | family | condition | success | tokens | turns | wall ms |");
  lines.push("|---|---|---|---|---|---|---|");
  for (const r of report.rows) {
    lines.push(
      `| ${r.id} | ${r.family} | ${r.condition} | ${r.success} | ${r.tokens_total} | ${r.turns} | ${r.wall_ms} |`,
    );
  }
  lines.push("");
  return lines.join("\n");
}

function writeReport(args: Args, report: Report): void {
  const jsonPath = args.out ?? join(reportsDir, `agentbench-${report.date}.json`);
  const mdPath = jsonPath.replace(/\.json$/, ".md");
  mkdirSync(dirname(jsonPath), { recursive: true });
  writeFileSync(jsonPath, `${JSON.stringify(report, null, 2)}\n`);
  writeFileSync(mdPath, markdown(report));
  process.stderr.write(`wrote ${jsonPath}\nwrote ${mdPath}\n`);
}

main().catch((err) => {
  process.stderr.write(`error: ${err instanceof Error ? err.message : String(err)}\n`);
  process.exit(1);
});
