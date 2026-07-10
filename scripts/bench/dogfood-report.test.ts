// Tests for the dogfood telemetry reporter. Run with `bun test scripts/bench`.

import { expect, test } from "bun:test";
import { computeReport, parseJsonl } from "./dogfood-report.ts";

test("parseJsonl skips blank and malformed lines", () => {
  const text = [
    '{"ts":"2026-07-01T00:00:00Z","event":"daemon_start"}',
    "",
    "   ",
    "{ this is not json",
    '{"ts":"2026-07-01T00:05:00Z","event":"daemon_stop"}',
    '{"truncated":', // a partially-flushed final line
  ].join("\n");
  const records = parseJsonl(text);
  expect(records.length).toBe(2);
  expect(records[0].event).toBe("daemon_start");
  expect(records[1].event).toBe("daemon_stop");
});

test("computeReport counts sessions and median duration", () => {
  const daemon = [
    { ts: "2026-07-01T10:00:00Z", event: "daemon_start", backend: "docker", mounts: 0 },
    { ts: "2026-07-01T10:00:10Z", event: "frontend_serving", backend: "docker", mounts: 2 },
    { ts: "2026-07-01T10:00:30Z", event: "frontend_stopped", backend: "docker", mounts: 2 },
    { ts: "2026-07-01T10:00:30Z", event: "daemon_stop", backend: "docker", mounts: 2 },
    { ts: "2026-07-01T12:00:00Z", event: "daemon_start", backend: "docker", mounts: 0 },
    { ts: "2026-07-01T12:00:50Z", event: "daemon_stop", backend: "docker", mounts: 1 },
  ];
  const report = computeReport(daemon, [], Date.parse("2026-07-01T13:00:00Z"));
  expect(report.sessions).toBe(2);
  expect(report.completedSessions).toBe(2);
  // durations: 30s and 50s -> median 40s.
  expect(report.medianSessionMs).toBe(40_000);
  expect(report.recoveries).toBe(0);
});

test("computeReport detects an unclean restart as a recovery", () => {
  // frontend_stopped with NO daemon_stop before the next daemon_start, within 5m.
  const daemon = [
    { ts: "2026-07-02T10:00:00Z", event: "daemon_start" },
    { ts: "2026-07-02T10:00:05Z", event: "frontend_serving" },
    { ts: "2026-07-02T10:05:00Z", event: "frontend_stopped" },
    // no daemon_stop here; the process died and was restarted 2 minutes later
    { ts: "2026-07-02T10:07:00Z", event: "daemon_start" },
    { ts: "2026-07-02T10:10:00Z", event: "daemon_stop" },
  ];
  const report = computeReport(daemon, [], Date.parse("2026-07-02T11:00:00Z"));
  expect(report.sessions).toBe(2);
  expect(report.completedSessions).toBe(1);
  expect(report.medianSessionMs).toBe(3 * 60 * 1000);
  expect(report.recoveries).toBe(1);
});

test("computeReport: a clean stop before restart is not a recovery", () => {
  const daemon = [
    { ts: "2026-07-02T10:00:00Z", event: "daemon_start" },
    { ts: "2026-07-02T10:05:00Z", event: "frontend_stopped" },
    { ts: "2026-07-02T10:05:00Z", event: "daemon_stop" },
    { ts: "2026-07-02T10:06:00Z", event: "daemon_start" },
    { ts: "2026-07-02T10:10:00Z", event: "daemon_stop" },
  ];
  const report = computeReport(daemon, [], Date.parse("2026-07-02T11:00:00Z"));
  expect(report.recoveries).toBe(0);
});

test("computeReport: restart after 5 minutes is not a recovery", () => {
  const daemon = [
    { ts: "2026-07-02T10:00:00Z", event: "daemon_start" },
    { ts: "2026-07-02T10:05:00Z", event: "frontend_stopped" },
    { ts: "2026-07-02T10:20:00Z", event: "daemon_start" }, // 15m later
  ];
  const report = computeReport(daemon, [], Date.parse("2026-07-02T11:00:00Z"));
  expect(report.recoveries).toBe(0);
});

test("computeReport counts weekly-active days and CLI usage", () => {
  const now = Date.parse("2026-07-08T12:00:00Z");
  const daemon = [
    { ts: "2026-07-01T12:00:00Z", event: "daemon_start" }, // exactly now-7d (inclusive)
    { ts: "2026-07-07T10:00:00Z", event: "daemon_start" },
  ];
  const cli = [
    { ts: "2026-06-01T10:00:00Z", cmd: "status", exit: 0 }, // outside the window
    { ts: "2026-07-07T11:00:00Z", cmd: "up", exit: 0 },
    { ts: "2026-07-08T09:00:00Z", cmd: "status", exit: 0 },
    { ts: "2026-07-08T09:05:00Z", cmd: "status", exit: 1 },
  ];
  const report = computeReport(daemon, cli, now);
  // Active days in the trailing week: 07-01 (boundary), 07-07, 07-08.
  expect(report.weeklyActiveDays).toBe(3);
  expect(report.cliInvocations).toBe(4);
  expect(report.cliByCommand.status).toBe(3);
  expect(report.cliByCommand.up).toBe(1);
});
