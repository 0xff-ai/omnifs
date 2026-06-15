---
title: Testing and debugging
description: Validate providers by compiling for wasm32-wasip2, running the dev runtime, and exercising the path surface with normal filesystem tools.
---

Provider testing needs two levels: code checks and filesystem behavior.

## Compile the provider

The repo-level provider gate is:

```bash
just providers-check
```

That gate sets up the WASI environment, runs provider clippy, and checks provider test compilation. The underlying checks look like:

```bash
cargo check -p omnifs-provider-db --target wasm32-wasip2
cargo clippy -p 'omnifs-provider-*' -p 'omnifs-tool-*' -p test-provider --target wasm32-wasip2 -- -D warnings
cargo test -p omnifs-provider-db --target wasm32-wasip2 --no-run
```

WASM provider tests can compile on the host test harness but do not execute without a WASM runtime. Use `--no-run` for target-specific compilation checks. Tests that need to execute should use `#[cfg(test)]` with host-target-compatible code. Providers do not carry in-crate `#[cfg(test)]` modules; verify provider behavior through host-driven integration tests in `crates/host/tests/`.

## Run the dev runtime

From the omnifs source checkout:

```bash
omnifs dev -y
omnifs status
omnifs shell
```

`omnifs dev` builds the dev image, wires provider manifests from the source checkout, materializes contributor fixtures and credentials, and starts the runtime with workspace providers available under `/omnifs`.

## Exercise the tree

Test the whole traversal path, not only the final file:

```bash
ls /omnifs
find /omnifs/dns -maxdepth 2 -type f | head
cat /omnifs/dns/example.com/MX
cat /omnifs/docker/system/version.json | jq .
cat /omnifs/db/tables/Album/schema.sql
```

For provider path changes, check each intermediate directory. Literal route prefixes must be navigable. Dynamic captures must not accidentally shadow static control directories.

## Use ordinary tools

A provider should behave like files for common tools:

```bash
ls -l /omnifs/github/0xff-ai/omnifs/issues/open
cat /omnifs/arxiv/papers/1706.03762/paper.json | jq .
grep -R "bug" /omnifs/linear/teams/ENG/issues/open
find /omnifs/docker/containers -maxdepth 3 -type f
```

The goal is not to prove every API endpoint. The goal is to prove the filesystem surface is coherent.

## Debug failures

```bash
omnifs logs -f
omnifs inspect --plain
omnifs status --detail
```

Look for failed callouts, denied capabilities, stale cache writes, missing credentials, and route capture parse failures.
