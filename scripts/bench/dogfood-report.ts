#!/usr/bin/env bun
//
// Dogfood telemetry reporter.
//
// Reads the workspace-local, never-transmitted telemetry JSONL written by the
// daemon and CLI (see `omnifs_workspace::telemetry`) and reports mount sessions
// and manual recoveries for the manual-recovery rate, plus weekly-active use.
// This reader only reads local files; it performs no network I/O.
//
// Usage:
//   bun scripts/bench/dogfood-report.ts [--home <OMNIFS_HOME>] [--json]
//
// `--home` defaults to $OMNIFS_HOME, then ~/.omnifs.

import { existsSync, readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

// Mirrors `omnifs_workspace::telemetry::TELEMETRY_SUBDIR` and the on-disk file
// names.
const TELEMETRY_SUBDIR = "telemetry";
const DAEMON_FILE = "daemon.jsonl";
const CLI_FILE = "cli.jsonl";

const RECOVERY_WINDOW_MS = 5 * 60 * 1000;
const WEEK_MS = 7 * 24 * 60 * 60 * 1000;

/**
 * Parse newline-delimited JSON, tolerating blank and malformed lines (a
 * truncated final write, or a partially-flushed record, must not abort the
 * report). Malformed lines are silently skipped.
 */
export function parseJsonl(text) {
  const records = [];
  for (const line of text.split("\n")) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    try {
      records.push(JSON.parse(trimmed));
    } catch {
      // Skip: a partial or corrupt line is expected at the tail of an
      // append-only log that a process was killed mid-write.
    }
  }
  return records;
}

function tsMs(record) {
  const parsed = Date.parse(record?.ts);
  return Number.isNaN(parsed) ? null : parsed;
}

function median(values) {
  if (values.length === 0) return null;
  const sorted = [...values].sort((a, b) => a - b);
  const mid = Math.floor(sorted.length / 2);
  return sorted.length % 2 ? sorted[mid] : (sorted[mid - 1] + sorted[mid]) / 2;
}

function dayKey(ms) {
  return new Date(ms).toISOString().slice(0, 10); // YYYY-MM-DD in UTC
}

/**
 * Compute the dogfood report from parsed daemon and CLI records.
 *
 * - sessions: number of `daemon_start` events.
 * - medianSessionMs: median duration from each `daemon_start` to the first
 *   `daemon_stop` before the next `daemon_start` (unfinished sessions excluded).
 * - recoveries: a `frontend_stopped` with no `daemon_stop` before the next
 *   `daemon_start`, where that next start lands within 5 minutes (an unplanned
 *   restart, counted toward the manual-recovery rate).
 * - weeklyActiveDays: distinct UTC days with any event in the trailing 7 days.
 */
export function computeReport(daemonRecords, cliRecords, now = Date.now()) {
  const events = daemonRecords
    .map((r) => ({ t: tsMs(r), event: r.event }))
    .filter((e) => e.t !== null && typeof e.event === "string")
    .sort((a, b) => a.t - b.t);

  let sessions = 0;
  const durations = [];
  let sessionStart = null;
  for (const event of events) {
    if (event.event === "daemon_start") {
      sessions++;
      sessionStart = event.t;
    } else if (event.event === "daemon_stop" && sessionStart !== null) {
      durations.push(event.t - sessionStart);
      sessionStart = null;
    }
  }

  let recoveries = 0;
  for (let i = 0; i < events.length; i++) {
    if (events[i].event !== "frontend_stopped") continue;
    for (let j = i + 1; j < events.length; j++) {
      if (events[j].event === "daemon_stop") {
        break;
      }
      if (events[j].event === "daemon_start") {
        if (events[j].t - events[i].t <= RECOVERY_WINDOW_MS) {
          recoveries++;
        }
        break;
      }
    }
  }

  const cliTimes = cliRecords.map(tsMs).filter((t) => t !== null);
  const allTimes = [...events.map((e) => e.t), ...cliTimes];
  const activeDays = new Set(allTimes.map(dayKey));
  const weeklyActiveDays = new Set(
    allTimes.filter((t) => t >= now - WEEK_MS && t <= now).map(dayKey),
  );

  const cliByCommand = {};
  for (const r of cliRecords) {
    if (typeof r?.cmd !== "string") continue;
    cliByCommand[r.cmd] = (cliByCommand[r.cmd] ?? 0) + 1;
  }

  return {
    sessions,
    completedSessions: durations.length,
    medianSessionMs: median(durations),
    recoveries,
    activeDays: activeDays.size,
    weeklyActiveDays: weeklyActiveDays.size,
    cliInvocations: cliTimes.length,
    cliByCommand,
  };
}

function formatDuration(ms) {
  if (ms === null) return "n/a";
  if (ms < 1000) return `${ms} ms`;
  const seconds = ms / 1000;
  if (seconds < 90) return `${seconds.toFixed(1)} s`;
  const minutes = seconds / 60;
  if (minutes < 90) return `${minutes.toFixed(1)} min`;
  return `${(minutes / 60).toFixed(1)} h`;
}

function readRecords(path) {
  if (!existsSync(path)) return [];
  return parseJsonl(readFileSync(path, "utf8"));
}

function parseArgs(argv) {
  const options = { home: process.env.OMNIFS_HOME || join(homedir(), ".omnifs"), json: false };
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    if (arg === "--home") {
      options.home = argv[++i];
    } else if (arg.startsWith("--home=")) {
      options.home = arg.slice("--home=".length);
    } else if (arg === "--json") {
      options.json = true;
    } else if (arg === "-h" || arg === "--help") {
      options.help = true;
    } else {
      throw new Error(`unknown argument: ${arg}`);
    }
  }
  return options;
}

function main() {
  const options = parseArgs(Bun.argv.slice(2));
  if (options.help) {
    console.log("usage: bun scripts/bench/dogfood-report.ts [--home <OMNIFS_HOME>] [--json]");
    return;
  }

  const telemetryDir = join(options.home, TELEMETRY_SUBDIR);
  const daemonRecords = readRecords(join(telemetryDir, DAEMON_FILE));
  const cliRecords = readRecords(join(telemetryDir, CLI_FILE));
  const report = computeReport(daemonRecords, cliRecords);

  if (options.json) {
    console.log(JSON.stringify(report, null, 2));
    return;
  }

  console.log(`omnifs dogfood report (${telemetryDir})`);
  console.log("");
  console.log(`  daemon sessions:        ${report.sessions}`);
  console.log(`  completed sessions:     ${report.completedSessions}`);
  console.log(`  median session length:  ${formatDuration(report.medianSessionMs)}`);
  console.log(`  manual recoveries:      ${report.recoveries}`);
  console.log(`  active days (all time): ${report.activeDays}`);
  console.log(`  weekly-active days:     ${report.weeklyActiveDays}`);
  console.log(`  CLI invocations:        ${report.cliInvocations}`);
  const commands = Object.entries(report.cliByCommand).sort((a, b) => b[1] - a[1]);
  if (commands.length > 0) {
    console.log("  CLI by command:");
    for (const [cmd, count] of commands) {
      console.log(`    ${cmd.padEnd(14)} ${count}`);
    }
  }
}

if (import.meta.main) {
  main();
}
