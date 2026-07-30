// Tests for the dogfood metrics reporter. Run with `bun test scripts/bench`.

import { expect, test } from "bun:test";
import { computeReport, parseJsonl } from "./dogfood-report.ts";

test("parseJsonl skips blank and malformed lines", () => {
  const text = [
    '{"ts":"2026-07-01T00:00:00Z","cmd":"status","exit":0}',
    "",
    "   ",
    "{ this is not json",
    '{"ts":"2026-07-01T00:05:00Z","cmd":"down","exit":0}',
    '{"truncated":', // a partially-flushed final line
  ].join("\n");
  const records = parseJsonl(text);
  expect(records.length).toBe(2);
  expect(records[0].cmd).toBe("status");
  expect(records[1].cmd).toBe("down");
});

test("computeReport counts weekly-active days and CLI usage", () => {
  const now = Date.parse("2026-07-08T12:00:00Z");
  const cli = [
    { ts: "2026-06-01T10:00:00Z", cmd: "status", exit: 0 },
    { ts: "2026-07-01T12:00:00Z", cmd: "mount", exit: 0 },
    { ts: "2026-07-07T11:00:00Z", cmd: "fs", exit: 0 },
    { ts: "2026-07-08T09:00:00Z", cmd: "status", exit: 0 },
    { ts: "2026-07-08T09:05:00Z", cmd: "status", exit: 1 },
  ];
  const report = computeReport(cli, now);
  expect(report.weeklyActiveDays).toBe(3);
  expect(report.cliInvocations).toBe(5);
  expect(report.cliByCommand.status).toBe(3);
  expect(report.cliByCommand.mount).toBe(1);
  expect(report.cliByCommand.fs).toBe(1);
});
