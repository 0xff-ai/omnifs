#!/usr/bin/env bun

import { Repo } from "../lib/repo";

const [key, ...rest] = Bun.argv.slice(2);
if (!key || rest.length > 0) {
  console.error("usage: scripts/toolchain/versions.ts KEY");
  process.exit(2);
}

const repo = await Repo.discover();
const versions = await repo.readToml<Record<string, unknown>>("tools", "versions.toml");
const value = versions[key];
if (value === undefined || value === null) {
  console.error(`missing key in tools/versions.toml: ${key}`);
  process.exit(1);
}
if (typeof value !== "string" && typeof value !== "number" && typeof value !== "boolean") {
  console.error(`expected scalar value for ${key}, got ${typeof value}`);
  process.exit(1);
}
console.log(String(value));
