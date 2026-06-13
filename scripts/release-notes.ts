#!/usr/bin/env bun

import { parseArgs, runCli } from "./lib/cli";
import { ReleaseWorkflow } from "./lib/release-workflow";
import { Repo } from "./lib/repo";

const USAGE = "usage: scripts/release-notes.ts draft --pr N [--force] | check --pr N | maintain";

await runCli(async () => {
  const { values, positionals } = parseArgs(Bun.argv.slice(2), {
    pr: { type: "string" },
    force: { type: "boolean" },
  });
  const [command = ""] = positionals;
  const release = new ReleaseWorkflow(await Repo.discover());

  if (command === "draft") {
    await release.draftPrReleaseNote(requirePr(values.pr), values.force === true);
    return;
  }
  if (command === "check") {
    await release.checkPrReleaseNote(requirePr(values.pr));
    return;
  }
  if (command === "maintain") {
    await release.maintainReleasePr();
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
