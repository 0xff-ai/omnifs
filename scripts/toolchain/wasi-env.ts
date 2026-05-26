#!/usr/bin/env bun

import { arch, platform } from "node:os";
import { dirname, join } from "node:path";
import { chdir, cwd } from "node:process";
import { Repo } from "../lib/repo";

type Versions = {
  wasi_sdk?: string;
  wasi_sdk_release?: string;
};

function hostSuffix(): string {
  const os = platform();
  const cpu = arch();
  if (os === "darwin" && cpu === "arm64") return "arm64-macos";
  if (os === "darwin" && cpu === "x64") return "x86_64-macos";
  if (os === "linux" && cpu === "arm64") return "arm64-linux";
  if (os === "linux" && cpu === "x64") return "x86_64-linux";
  throw new Error(`unsupported host for wasi-sdk install: ${os}-${cpu}`);
}

function shellQuote(value: string): string {
  return `'${value.replaceAll("'", "'\\''")}'`;
}

const repo = await Repo.discover(cwd());
chdir(repo.root);

const versions = await repo.readToml<Versions>("tools", "versions.toml");
const version = String(versions.wasi_sdk ?? "");
const release = String(versions.wasi_sdk_release ?? version);
if (!version || !release) {
  throw new Error("tools/versions.toml must define wasi_sdk and wasi_sdk_release");
}

const home = repo.path(".cache", `wasi-sdk-${release}`);
const sysroot = join(home, "share", "wasi-sysroot");
const clang = join(home, "bin", "clang");

if (!await Bun.file(join(sysroot, "include", "stdio.h")).exists()) {
  const tarball = `wasi-sdk-${release}-${hostSuffix()}.tar.gz`;
  const url = `https://github.com/WebAssembly/wasi-sdk/releases/download/wasi-sdk-${version}/${tarball}`;
  const archive = `${home}.tar.gz`;

  await repo.$`mkdir -p ${dirname(home)}`;
  await repo.$`curl -fsSL -o ${archive} ${url}`;
  await repo.$`rm -rf ${home}`;
  await repo.$`mkdir -p ${home}`;
  await repo.$`tar -xzf ${archive} -C ${home} --strip-components=1`;
  await repo.$`rm -f ${archive}`.nothrow().quiet();
}

console.log(`export WASI_SYSROOT=${shellQuote(sysroot)}`);
console.log(`export CC_wasm32_wasip2=${shellQuote(clang)}`);
console.log(`export CFLAGS_wasm32_wasip2=${shellQuote(`--sysroot=${sysroot}`)}`);
