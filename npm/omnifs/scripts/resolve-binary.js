"use strict";

const fs = require("node:fs");
const path = require("node:path");
const { optionalDependencies = {} } = require("../package.json");

function packageForPlatform(platform = process.platform, arch = process.arch) {
  const packageName = `@0xff-ai/omnifs-cli-${platform}-${arch}`;
  return Object.prototype.hasOwnProperty.call(optionalDependencies, packageName)
    ? packageName
    : null;
}

function existingExecutable(file) {
  try {
    const stat = fs.statSync(file);
    return stat.isFile();
  } catch {
    return false;
  }
}

function resolveBinary() {
  if (process.env.OMNIFS_BIN) {
    if (existingExecutable(process.env.OMNIFS_BIN)) {
      return { ok: true, path: process.env.OMNIFS_BIN };
    }
    return {
      ok: false,
      message: `OMNIFS_BIN points to a missing binary: ${process.env.OMNIFS_BIN}`
    };
  }

  const packageName = packageForPlatform();
  if (!packageName) {
    return {
      ok: false,
      message: `omnifs does not ship an npm CLI binary for ${process.platform}/${process.arch}`
    };
  }

  try {
    const packageJson = require.resolve(`${packageName}/package.json`);
    const binary = path.join(path.dirname(packageJson), "bin", "omnifs");
    if (existingExecutable(binary)) {
      return { ok: true, path: binary };
    }
    return {
      ok: false,
      message: [
        `${packageName} is installed, but ${binary} is missing.`,
        "This usually means an interrupted or partial upgrade left the platform package without its binary.",
        "Fix it with:",
        "  npm uninstall -g @0xff-ai/omnifs",
        "  npm install -g @0xff-ai/omnifs@dev    # or @latest"
      ].join("\n")
    };
  } catch {
    return {
      ok: false,
      message: [
        `missing platform package ${packageName}`,
        "Reinstall omnifs so npm can fetch the optional platform package.",
        "For local development, set OMNIFS_BIN to a built omnifs binary."
      ].join("\n")
    };
  }
}

module.exports = {
  packageForPlatform,
  resolveBinary
};
