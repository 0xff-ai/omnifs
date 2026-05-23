#!/usr/bin/env node
import fs from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const npmRoot = path.join(repoRoot, "npm");
const packageFiles = [
  path.join(npmRoot, "omnifs", "package.json"),
  path.join(npmRoot, "platform", "darwin-arm64", "package.json"),
  path.join(npmRoot, "platform", "darwin-x64", "package.json"),
  path.join(npmRoot, "platform", "linux-arm64", "package.json"),
  path.join(npmRoot, "platform", "linux-x64", "package.json")
];

function workspaceVersion() {
  const cargoToml = fs.readFileSync(path.join(repoRoot, "Cargo.toml"), "utf8");
  const match = cargoToml.match(/\[workspace\.package\][\s\S]*?\nversion = "([^"]+)"/);
  if (!match) {
    throw new Error("could not find [workspace.package] version in Cargo.toml");
  }
  return match[1];
}

const version = process.argv[2] || workspaceVersion();

for (const file of packageFiles) {
  const packageJson = JSON.parse(fs.readFileSync(file, "utf8"));
  packageJson.version = version;
  if (packageJson.name === "@0xff-ai/omnifs") {
    for (const dependency of Object.keys(packageJson.optionalDependencies || {})) {
      packageJson.optionalDependencies[dependency] = version;
    }
  }
  fs.writeFileSync(file, `${JSON.stringify(packageJson, null, 2)}\n`);
}
