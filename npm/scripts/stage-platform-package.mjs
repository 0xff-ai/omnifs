#!/usr/bin/env node
import fs from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const platform = process.argv[2];
const sourceBinary = process.argv[3];

if (!platform || !sourceBinary) {
  console.error("usage: stage-platform-package.mjs <platform> <source-binary>");
  process.exit(1);
}

const packageDir = path.join(repoRoot, "npm", "platform", platform);
const packageJson = path.join(packageDir, "package.json");
if (!fs.existsSync(packageJson)) {
  console.error(`unknown npm platform package: ${platform}`);
  process.exit(1);
}

if (!fs.existsSync(sourceBinary)) {
  console.error(`source binary does not exist: ${sourceBinary}`);
  process.exit(1);
}

const binDir = path.join(packageDir, "bin");
fs.mkdirSync(binDir, { recursive: true });
const targetBinary = path.join(binDir, path.basename(sourceBinary));
fs.copyFileSync(sourceBinary, targetBinary);
fs.chmodSync(targetBinary, 0o755);

console.log(`staged ${sourceBinary} -> ${targetBinary}`);
