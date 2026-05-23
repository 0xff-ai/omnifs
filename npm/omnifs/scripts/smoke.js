"use strict";

const { spawnSync } = require("node:child_process");
const { resolveBinary } = require("./resolve-binary");

const resolved = resolveBinary();
if (!resolved.ok) {
  console.error(resolved.message);
  process.exit(1);
}

const result = spawnSync(resolved.path, ["--version"], {
  encoding: "utf8",
  stdio: ["ignore", "pipe", "pipe"]
});

if (result.error) {
  console.error(result.error.message);
  process.exit(1);
}

if (result.status !== 0) {
  process.stderr.write(result.stderr);
  process.exit(result.status || 1);
}

if (!/^omnifs /.test(result.stdout.trim())) {
  console.error(`unexpected version output: ${result.stdout.trim()}`);
  process.exit(1);
}

process.stdout.write(result.stdout);
