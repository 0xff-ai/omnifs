"use strict";

const fs = require("node:fs");
const path = require("node:path");

const platforms = require("../../platforms.json");
const PACKAGES = Object.fromEntries(
  Object.values(platforms).map((spec) => [`${spec.os}:${spec.cpu}`, spec.package])
);

function packageForPlatform(platform = process.platform, arch = process.arch) {
  return PACKAGES[`${platform}:${arch}`] || null;
}

function binaryName() {
  return process.platform === "win32" ? "omnifs.exe" : "omnifs";
}

function existingExecutable(file) {
  try {
    const stat = fs.statSync(file);
    return stat.isFile();
  } catch (_) {
    return false;
  }
}

function resolveFromPackage(packageName) {
  const packageJson = require.resolve(`${packageName}/package.json`);
  const packageRoot = path.dirname(packageJson);
  return path.join(packageRoot, "bin", binaryName());
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
    const binary = resolveFromPackage(packageName);
    if (existingExecutable(binary)) {
      return { ok: true, path: binary };
    }
    return {
      ok: false,
      message: `${packageName} is installed, but ${binary} is missing`
    };
  } catch (error) {
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
