#!/usr/bin/env bun

import { mkdir } from "node:fs/promises";
import { dirname } from "node:path";
import { parseArgs, runCli } from "./lib/cli";
import { Repo } from "./lib/repo";

const USAGE = "usage: scripts/openapi.ts generate | check";

const SPEC_PATH = "crates/omnifs-api/openapi/daemon.json";

await runCli(async () => {
  const { positionals } = parseArgs(Bun.argv.slice(2), {});
  const [command, ...rest] = positionals;
  if (rest.length > 0) throw new Error(`unexpected argument: ${rest[0]}`);

  const workspace = new OpenApiWorkspace(await Repo.discover());
  if (command === "generate") {
    await workspace.generate();
    return;
  }
  if (command === "check") {
    await workspace.check();
    return;
  }
  throw new Error(USAGE);
});

class OpenApiWorkspace {
  constructor(private readonly repo: Repo) {}

  async generate(): Promise<void> {
    await this.writeFileIfChanged(SPEC_PATH, await this.specFromImplementation());
    console.log("OpenAPI spec written");
  }

  async check(): Promise<void> {
    const implemented = await this.specFromImplementation();
    const checkedIn = await Bun.file(this.repo.path(SPEC_PATH)).text();
    if (checkedIn !== implemented) {
      throw new Error(`checked-in OpenAPI spec is stale; run \`just openapi\``);
    }
    console.log("OpenAPI spec is current");
  }

  private async specFromImplementation(): Promise<string> {
    return await this.repo.$`cargo run -p omnifs-daemon --bin omnifsd-openapi --quiet`.text();
  }

  private async writeFileIfChanged(path: string, contents: string): Promise<void> {
    const file = Bun.file(this.repo.path(path));
    if (await file.exists() && await file.text() === contents) return;
    await mkdir(dirname(this.repo.path(path)), { recursive: true });
    await Bun.write(this.repo.path(path), contents);
  }
}
