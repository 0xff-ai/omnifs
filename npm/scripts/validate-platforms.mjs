#!/usr/bin/env node
import fs from "node:fs";
import path from "node:path";
import process from "node:process";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const require = createRequire(import.meta.url);
const platformsPath = path.join(repoRoot, "npm", "platforms.json");
const platforms = JSON.parse(fs.readFileSync(platformsPath, "utf8"));
const platformKeys = Object.keys(platforms);
const platformKeySet = new Set(platformKeys);
const errors = [];

function fail(message) {
  errors.push(message);
}

function readJson(relativePath) {
  const absolutePath = path.join(repoRoot, relativePath);
  return JSON.parse(fs.readFileSync(absolutePath, "utf8"));
}

function assertSetEqual(actual, expected, label, format = (value) => value) {
  const actualSet = new Set(actual);
  const expectedSet = new Set(expected);
  for (const value of actualSet) {
    if (!expectedSet.has(value)) {
      fail(`${label} has extra entry ${format(value)}`);
    }
  }
  for (const value of expectedSet) {
    if (!actualSet.has(value)) {
      fail(`${label} missing entry ${format(value)}`);
    }
  }
}

function parseDistTargets(toml) {
  const targets = [];
  let inTargets = false;
  for (const line of toml.split("\n")) {
    const trimmed = line.trim();
    if (trimmed === "targets = [") {
      inTargets = true;
      continue;
    }
    if (inTargets) {
      if (trimmed === "]") {
        break;
      }
      const match = trimmed.match(/^"([^"]+)",?$/);
      if (match) {
        targets.push(match[1]);
      }
    }
  }
  return targets;
}

function parseNpmWorkflowMatrix(yaml) {
  const entries = [];
  for (const block of yaml.split(/^\s+- package-platform:/m).slice(1)) {
    const platform = block.match(/^\s*(\S+)/)?.[1];
    const rustTarget = block.match(/^\s*rust-target:\s*(\S+)/m)?.[1];
    const runner = block.match(/^\s*runner:\s*(.+)$/m)?.[1]?.trim();
    if (platform && rustTarget && runner) {
      entries.push({ platform, rustTarget, runner });
    }
  }
  return entries;
}

for (const [platformKey, spec] of Object.entries(platforms)) {
  const required = ["package", "rustTarget", "os", "cpu", "runner"];
  for (const field of required) {
    if (!spec[field]) {
      fail(`platforms.json:${platformKey} missing ${field}`);
    }
  }

  const packageJsonPath = path.join("npm", "platform", platformKey, "package.json");
  if (!fs.existsSync(path.join(repoRoot, packageJsonPath))) {
    fail(`missing npm platform package directory for ${platformKey}`);
    continue;
  }

  const packageJson = readJson(packageJsonPath);
  if (packageJson.name !== spec.package) {
    fail(
      `${packageJsonPath} name ${packageJson.name} != platforms.json package ${spec.package}`
    );
  }
  if (JSON.stringify(packageJson.os) !== JSON.stringify([spec.os])) {
    fail(`${packageJsonPath} os mismatch for ${platformKey}`);
  }
  if (JSON.stringify(packageJson.cpu) !== JSON.stringify([spec.cpu])) {
    fail(`${packageJsonPath} cpu mismatch for ${platformKey}`);
  }
}

const platformDir = path.join(repoRoot, "npm", "platform");
for (const entry of fs.readdirSync(platformDir, { withFileTypes: true })) {
  if (entry.isDirectory() && !platformKeySet.has(entry.name)) {
    fail(`npm/platform/${entry.name} is not declared in platforms.json`);
  }
}

const rootPackage = readJson("npm/omnifs/package.json");
assertSetEqual(
  Object.keys(rootPackage.optionalDependencies ?? {}),
  Object.values(platforms).map((spec) => spec.package),
  "npm/omnifs/package.json optionalDependencies"
);

const resolveBinarySource = fs.readFileSync(
  path.join(repoRoot, "npm/omnifs/scripts/resolve-binary.js"),
  "utf8"
);
if (!resolveBinarySource.includes("platforms.json")) {
  fail("npm/omnifs/scripts/resolve-binary.js must load npm/platforms.json");
}

const { packageForPlatform } = require(
  path.join(repoRoot, "npm/omnifs/scripts/resolve-binary.js")
);
const resolveBinaryPackages = Object.fromEntries(
  Object.values(platforms).map((spec) => [`${spec.os}:${spec.cpu}`, spec.package])
);
for (const [platformArch, packageName] of Object.entries(resolveBinaryPackages)) {
  const [platform, arch] = platformArch.split(":");
  const resolved = packageForPlatform(platform, arch);
  if (resolved !== packageName) {
    fail(
      `resolve-binary.js maps ${platformArch} to ${resolved ?? "null"}, expected ${packageName}`
    );
  }
}
assertSetEqual(
  Object.keys(resolveBinaryPackages),
  Object.values(platforms).map((spec) => `${spec.os}:${spec.cpu}`),
  "resolve-binary platform map keys"
);

const distWorkspace = fs.readFileSync(path.join(repoRoot, "dist-workspace.toml"), "utf8");
assertSetEqual(
  parseDistTargets(distWorkspace),
  Object.values(platforms).map((spec) => spec.rustTarget),
  "dist-workspace.toml targets"
);

const npmWorkflow = fs.readFileSync(
  path.join(repoRoot, ".github/workflows/npm.yml"),
  "utf8"
);
const workflowMatrix = parseNpmWorkflowMatrix(npmWorkflow);
const workflowByPlatform = Object.fromEntries(
  workflowMatrix.map((entry) => [entry.platform, entry])
);

for (const platformKey of platformKeys) {
  const expected = platforms[platformKey];
  const actual = workflowByPlatform[platformKey];
  if (!actual) {
    fail(`.github/workflows/npm.yml missing matrix entry for ${platformKey}`);
    continue;
  }
  if (actual.rustTarget !== expected.rustTarget) {
    fail(
      `.github/workflows/npm.yml ${platformKey} rust-target ${actual.rustTarget} != platforms.json ${expected.rustTarget}`
    );
  }
  if (actual.runner !== expected.runner) {
    fail(
      `.github/workflows/npm.yml ${platformKey} runner ${actual.runner} != platforms.json ${expected.runner}`
    );
  }
}

for (const platformKey of Object.keys(workflowByPlatform)) {
  if (!platformKeySet.has(platformKey)) {
    fail(`.github/workflows/npm.yml has extra matrix entry ${platformKey}`);
  }
}

if (errors.length > 0) {
  for (const error of errors) {
    console.error(`error: ${error}`);
  }
  process.exit(1);
}

console.log(`validated ${platformKeys.length} npm platform entries`);
