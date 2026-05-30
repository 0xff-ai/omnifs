---
title: Browsing with standard tools
description: Use cd, ls, cat, grep, find, and jq to explore omnifs. Every projected path behaves like a real file.
---

omnifs projects external services into local paths. You do not learn a new
client for each service — you use the shell tools you already know. To explore a
provider, `cd` into its mount and read files.

```bash
cd /github/torvalds/linux
ls
cat README
```

## The "behaves like a real file" contract

Every omnifs path is judged against a single rule: it must behave like a real
file for the standard Linux toolbox. A directory lists with `ls`, a file reads
with `cat`, sizes show up in `stat`, and search tools recurse into the tree.
This is a hard design invariant, not a best effort — a change that makes any of
the tools below regress is treated as a bug.

Because the contract holds, you can compose omnifs paths with pipes, redirects,
and any other Unix tool exactly as you would with files on disk.

```bash
# Pipe a projected file straight into jq
cat /arxiv/papers/1706.03762/metadata.json | jq .title

# Grep across a cloned repo tree
grep -rn "TODO" /github/myorg/myrepo/repo/src

# Diff two DNS answers
diff <(cat /dns/example.com/A) <(cat /dns/@1.1.1.1/example.com/A)
```

## Supported toolbox

The following categories of tools are supported against omnifs paths.

| Category | Tools |
|----------|-------|
| Read content | `cat`, `head`, `tail` (incl. `-f`, `-n`, `-c`), `less`, `more`, `xxd`, `hexdump`, `od`, `file` |
| Search and traversal | `grep` (incl. `-r`), `rg`, `find` (incl. `-name`, `-size`, `-type`), `fd` |
| Stat-based | `ls` (incl. `-l`, `-h`), `du` (incl. `-sh`), `wc` (incl. `-l`, `-c`, `-m`), `stat` |
| Copy and archive | `cp`, `mv`, `tar` (`c`, `x`, `t`), `rsync` |
| Compare and hash | `diff`, `cmp`, `md5sum`, `sha256sum`, `b3sum` |
| Inspection | `jq`, `yq`, `xmllint` |
| Editors | `vim`, `neovim`, `nano` |

:::note
Editors that `mmap` files (including some `code` configurations) are
best-effort. They should not break, but the read-content and stat-based tools
above are the firm contract.
:::

## Navigating a tree

Use `ls` to discover what a directory holds and `cd` to descend. Directories
that contain dynamic children (for example a GitHub owner's repositories) list
what omnifs already knows and fetch more as you navigate.

```console
$ ls /
arxiv  db  dns  docker  github  linear

$ ls /github/rust-lang
cargo  crates.io  rust  rustlings  rustup

$ cd /github/rust-lang/rust
$ ls
actions  issues  pulls  repo
```

## Reading content

`cat`, `head`, and `tail` work on any projected file. `tail -f` follows files
that grow, such as CI logs or container logs.

```bash
cat /dns/google.com/MX
head -n 20 /github/rust-lang/rust/issues/100000/body
tail -f /docker/containers/<id>/logs
```

## Searching and filtering

`grep -r`, `rg`, and `find` recurse through projected trees, and `jq` parses the
JSON files that providers expose.

```bash
find /github/myorg/myrepo/repo -name '*.rs' -type f
grep -rn "panic!" /github/myorg/myrepo/repo/src
jq '.authors[]' /arxiv/papers/1706.03762/metadata.json
```

:::tip
If a tool behaves unexpectedly on a path, that is worth reporting — the goal is
that omnifs paths are indistinguishable from real files. See
[Inspect and debug](/guides/inspect-debug/) to trace what the runtime is doing.
:::
