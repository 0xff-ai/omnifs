---
title: Testing strategy
description: Native host tests, WASM provider compile-checks, the smoke harness and bash-tool compatibility invariant, runtime validation, and the test-quality bar.
---

Testing omnifs spans three layers: native host/CLI tests, WASM provider
compile-checks, and live runtime validation. The bash-tool compatibility
invariant ties them together - every change is judged against the standard Linux
toolbox.

## Native host tests

Host and CLI code runs through `cargo nextest`:

```bash
cargo fmt
cargo nextest run
```

`just check` runs the same tests across the workspace, excluding the WASM
packages (which are checked separately under their own target).

## WASM provider compile-checks

Provider tests compile under `wasm32-wasip2` but cannot execute - there is no
WASM runtime in the test harness. Use `--no-run` to get a compilation check:

```bash
cargo test -p 'omnifs-provider-*' -p test-provider --target wasm32-wasip2 --no-run
```

Tests that need to actually run must use `#[cfg(test)]` with
host-target-compatible code rather than relying on the WASM target.

## The smoke harness and bash-tool compatibility

omnifs paths must behave like real files for the standard Linux toolbox. This is
a hard invariant: a change that makes any of these categories regress is wrong.
Prove a new feature does not regress them through the smoke harness in
`tests/smoke/` or a unit test.

- **Read content**: `cat`, `head`, `tail` (including `-f`, `-n`, `-c`), `less`,
  `more`, `xxd`, `hexdump`, `od`, `file`
- **Search and traversal**: `grep` (including `-r`), `rg`, `find` (including
  `-name`, `-size`, `-type`), `fd`
- **Stat-based**: `ls` (including `-l`, `-h`), `du` (including `-sh`), `wc`
  (including `-l`, `-c`, `-m`), `stat`
- **Copy and archive**: `cp`, `mv`, `tar` (`c`, `x`, `t`), `rsync`
- **Compare and hash**: `diff`, `cmp`, `md5sum`, `sha256sum`, `b3sum`
- **Inspection**: `jq`, `yq`, `xmllint`
- **Editors**: `vim`, `neovim`, `nano` (mmap-based editors are best-effort but
  should not break)

You can stage smoke fixtures without launching the runtime:

```bash
just smoke-init
```

## Runtime validation

For mount, provider, clone, traversal, or runtime behavior changes, do not stop
at Rust-only checks. Validate through the supported runtime path:

```bash
omnifs dev -y
docker exec omnifs /bin/zsh -lc 'omnifs status'
docker exec omnifs /bin/zsh -lc 'OMNIFS_DEMO_MODE=smoke /tmp/demo.sh'
docker exec omnifs /bin/zsh -lc 'tail -n 80 /tmp/omnifs.log'
```

For path-surface changes, test the whole shell traversal, not only the intended
leaf paths. In the live container, run `ll`, `cd`, and `find` from the provider
root through every intermediate directory. Verify that:

- parent directories do not synthesize duplicate root entries,
- route scaffolding names do not bind as dynamic captures,
- control directories do not contain paper/item nodes unless the design says so.

:::tip
When debugging a slow or broken path, start with user-visible probes before
theory: `cd /github/<owner>`, `cat /dns/@google/<domain>/MX`, then
`tail -n 80 /tmp/omnifs.log`. When a repo path returns `Input/output error`,
check `omnifs logs`, SSH auth inside the container, and whether the mount is
still in `/proc/mounts`.
:::

## The test-quality bar

Prefer tests that protect behavior the project actually depends on: user-visible
workflows, domain invariants, security/auth boundaries, persistence and
wire-format compatibility, or regressions that would be easy to reintroduce.

Avoid tests whose main value is confirming local plumbing or library behavior:

- serde/clap/url/http/std helpers working as documented
- a wrapper forwarding fields unchanged
- a builder storing the exact values just passed to it
- an in-memory fake round-tripping data without exercising caller behavior
- brittle presentation-text checks for non-contractual wording

Small unit tests are fine when they guard a non-obvious rule, an edge case with
real product meaning, or a boundary that is hard to exercise elsewhere. Before
adding a narrow test, be able to say what regression it would catch and why that
regression matters.
