#!/usr/bin/env bun
//
// Dogfood metrics reporter.
//
// Reads the profile-local, never-transmitted CLI metrics JSONL and reports
// command use plus weekly-active days.
// This reader only reads local files; it performs no network I/O.
//
// Usage:
//   bun scripts/bench/dogfood-report.ts [--home <OMNIFS_HOME>] [--json]
//
// `--home` defaults to $OMNIFS_HOME, then ~/.omnifs.

import { existsSync, readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

const METRICS_SUBDIR = "metrics";
const CLI_FILE = "cli.jsonl";

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

function dayKey(ms) {
  return new Date(ms).toISOString().slice(0, 10); // YYYY-MM-DD in UTC
}

/**
 * Compute the dogfood report from parsed CLI records.
 */
export function computeReport(cliRecords, now = Date.now()) {
  const cliTimes = cliRecords.map(tsMs).filter((t) => t !== null);
  const activeDays = new Set(cliTimes.map(dayKey));
  const weeklyActiveDays = new Set(
    cliTimes.filter((t) => t >= now - WEEK_MS && t <= now).map(dayKey),
  );

  const cliByCommand = {};
  for (const r of cliRecords) {
    if (typeof r?.cmd !== "string") continue;
    cliByCommand[r.cmd] = (cliByCommand[r.cmd] ?? 0) + 1;
  }

  return {
    activeDays: activeDays.size,
    weeklyActiveDays: weeklyActiveDays.size,
    cliInvocations: cliTimes.length,
    cliByCommand,
  };
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

  const metricsDir = join(options.home, METRICS_SUBDIR);
  const cliRecords = readRecords(join(metricsDir, CLI_FILE));
  const report = computeReport(cliRecords);

  if (options.json) {
    console.log(JSON.stringify(report, null, 2));
    return;
  }

  console.log(`omnifs dogfood report (${metricsDir})`);
  console.log("");
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
