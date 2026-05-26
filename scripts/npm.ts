#!/usr/bin/env bun

import { parseArgs, runCli, takeCommandWithVersion } from "./lib/cli";
import { NpmWorkspace } from "./lib/npm-workspace";
import { Repo } from "./lib/repo";

const USAGE = "usage: scripts/npm.ts sync [VERSION] [--version VERSION] | validate";

await runCli(async () => {
  const { values, positionals } = parseArgs(Bun.argv.slice(2), {
    version: { type: "string" },
  });
  const { command, version } = takeCommandWithVersion(values, positionals);
  const npm = new NpmWorkspace(await Repo.discover());

  if (command === "sync") {
    await npm.sync(version);
    return;
  }
  if (command === "validate") {
    if (version) throw new Error("validate does not accept a version argument");
    const entries = await npm.validate();
    console.log(`validated ${entries} npm platform entries`);
    return;
  }
  throw new Error(USAGE);
});
