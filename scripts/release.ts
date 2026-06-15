#!/usr/bin/env bun

import { parseArgs, runCli, takeCommandWithVersion } from "./lib/cli";
import { ReleaseWorkflow, type ShipPlan } from "./lib/release-workflow";
import { Repo } from "./lib/repo";

const USAGE = "usage: scripts/release.ts check | plan [--format text|json] | cut [VERSION] [--version VERSION] [--no-push]";

await runCli(async () => {
  const { values, positionals } = parseArgs(Bun.argv.slice(2), {
    format: { type: "string" },
    version: { type: "string" },
    "no-push": { type: "boolean" },
  });
  const { command, version } = takeCommandWithVersion(values, positionals);
  const format = typeof values.format === "string" ? values.format : "text";
  if (format !== "text" && format !== "json") {
    throw new Error(`invalid format: ${format}`);
  }
  const release = new ReleaseWorkflow(await Repo.discover());

  if (command === "check") {
    await release.releaseCheck();
    return;
  }
  if (command === "plan") {
    printPlan(await release.shipPlan(), format);
    return;
  }
  if (command === "cut") {
    const push = values["no-push"] !== true;
    await release.releaseCut(version, push);
    return;
  }
  throw new Error(USAGE);
});

function printPlan(plan: ShipPlan, format: "text" | "json"): void {
  if (format === "json") {
    console.log(JSON.stringify(plan, null, 2));
    return;
  }
  if (plan.should_ship) {
    console.log("should_ship=true");
    console.log(`version=${plan.version}`);
    console.log(`tag=${plan.tag}`);
    return;
  }
  console.log("should_ship=false");
  console.log(`version=${plan.version}`);
}
