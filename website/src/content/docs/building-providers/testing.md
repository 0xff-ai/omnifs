---
title: Testing Providers
description: Building and checking providers for wasm32-wasip2, the --no-run constraint, host-target unit tests, and bash-tool compatibility.
---

Provider checks differ from host checks in one essential way: providers target `wasm32-wasip2`, and there is no WASM runtime in the test harness. So you compile provider tests for the target but cannot execute them there. Plan your tests around that constraint.

## Clippy and compile checks

Provider clippy and test commands must include `--target wasm32-wasip2` and use the package globs. Run clippy with warnings denied:

```bash
cargo clippy -p 'omnifs-provider-*' -p 'omnifs-tool-*' -p test-provider \
  --target wasm32-wasip2 -- -D warnings
```

Compile the tests for the target without running them:

```bash
cargo test -p 'omnifs-provider-*' -p test-provider \
  --target wasm32-wasip2 --no-run
```

`--no-run` is mandatory for target-specific provider tests: the harness can build them but cannot execute a `wasm32-wasip2` binary. A passing `--no-run` proves the test code compiles for the real target.

The convenience recipes wrap these:

```bash
just providers-check   # wasm32-wasip2 check + clippy for providers and tools
just providers-build   # release-build providers and tools for wasm32-wasip2
```

## Tests that must actually run

For logic you want to execute as a test, write host-target-compatible code under `#[cfg(test)]`. Keep that code free of WIT/callout dependencies so it builds and runs on the native test target:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arxiv_id_strips_url_prefix() {
        assert_eq!(arxiv_id("http://arxiv.org/abs/2401.00001v1"), "2401.00001v1");
    }
}
```

Pure helpers — parsers, formatters, path logic — are the right thing to unit-test this way. Run them with the host baseline:

```bash
cargo nextest run
```

Anything that needs a `Cx`, a callout, or the WIT boundary cannot run in the harness; verify those through the live runtime instead (below).

## What to test, and what not to

Prefer tests that protect behavior the project depends on: a path parser that must reject malformed ids, a renderer whose output is a stable wire format, a projection rule with real product meaning. Before adding a narrow test, be able to name the regression it catches.

Avoid tests whose only value is confirming library plumbing: serde round-tripping, a builder storing the value you just passed, an in-memory fake echoing data. Those add maintenance cost without protecting behavior.

## Bash-tool compatibility

omnifs paths must behave like real files for the standard Linux toolbox — `cat`, `head`, `tail -f`, `grep -r`, `find`, `ls -l`, `du -sh`, `stat`, `cp`, `tar`, `diff`, `sha256sum`, `jq`, and editors. A provider change that makes any of these regress is wrong. The smoke harness in `tests/smoke/` exercises these tools against live mounts; prove a new feature does not regress them there.

## Live runtime validation

For anything touching mount, traversal, clone, or runtime behavior, do not stop at Rust checks. Bring up the dev container and exercise the real path:

```bash
omnifs dev -y
docker exec omnifs /bin/zsh -lc 'omnifs status'
docker exec omnifs /bin/zsh -lc 'OMNIFS_DEMO_MODE=smoke /tmp/demo.sh'
docker exec omnifs /bin/zsh -lc 'tail -n 80 /tmp/omnifs.log'
```

For path-surface changes, walk the whole traversal — `ll`, `cd`, and `find` from the provider root through every intermediate directory — not just the leaf paths you added. Verify that parent directories do not synthesize duplicate root entries, that route scaffolding names do not bind as dynamic captures, and that control directories do not contain stray nodes.

:::tip
The runtime log inside the container is `/tmp/omnifs.log`. When a path returns `Input/output error`, check the log first, then auth, then whether the mount is still present in `/proc/mounts`.
:::

:::caution
Do not assume `docker exec` inherits the entrypoint environment. Verify live runtime paths directly rather than inferring them from defaults.
:::
