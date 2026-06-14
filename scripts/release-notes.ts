#!/usr/bin/env bun

import { ChangelogBot } from "./lib/changelog-bot";
import { parseArgs, runCli } from "./lib/cli";
import { Repo } from "./lib/repo";

const USAGE = "usage: scripts/release-notes.ts draft --pr N | record";

await runCli(async () => {
  const { values, positionals } = parseArgs(Bun.argv.slice(2), {
    pr: { type: "string" },
  });
  const [command = ""] = positionals;
  const bot = new ChangelogBot(await Repo.discover());

  if (command === "draft") {
    await bot.draft(requirePr(values.pr));
    return;
  }
  if (command === "record") {
    await bot.record();
    return;
  }
  throw new Error(USAGE);
});

function requirePr(value: unknown): number {
  const pr = typeof value === "string" ? Number(value) : NaN;
  if (!Number.isInteger(pr) || pr <= 0) {
    throw new Error("--pr must be a positive PR number");
  }
  return pr;
}
