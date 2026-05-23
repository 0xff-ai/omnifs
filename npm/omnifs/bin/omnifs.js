#!/usr/bin/env node
"use strict";

const { spawnSync } = require("node:child_process");
const { resolveBinary } = require("../scripts/resolve-binary");

const resolved = resolveBinary();
if (!resolved.ok) {
  console.error(resolved.message);
  process.exit(1);
}

const child = spawnSync(resolved.path, process.argv.slice(2), {
  stdio: "inherit",
  windowsHide: true
});

if (child.error) {
  console.error(`failed to run ${resolved.path}: ${child.error.message}`);
  process.exit(1);
}

if (child.signal) {
  process.kill(process.pid, child.signal);
  return;
}

process.exit(child.status === null ? 1 : child.status);
