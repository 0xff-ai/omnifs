---
title: Shell compatibility
description: The promise that omnifs paths behave like real files for standard Unix tools, and where that guarantee holds today.
---

# Shell compatibility

omnifs paths must behave like real files for the standard Unix toolbox. This is a hard invariant, not a design aspiration. Every change to the projection is judged against the same list, and a change that makes any of these regress is wrong.

## The toolbox

| Family | Tools |
|---|---|
| Read content | `cat`, `head`, `tail` (including `-f`, `-n`, `-c`), `less`, `more`, `xxd`, `hexdump`, `od`, `file` |
| Search and traversal | `grep` (including `-r`), `rg`, `find` (including `-name`, `-size`, `-type`), `fd` |
| Stat-based | `ls` (including `-l`, `-h`), `du` (including `-sh`), `wc` (including `-l`, `-c`, `-m`), `stat` |
| Copy and archive | `cp`, `mv`, `tar` (`c`, `x`, `t`), `rsync` |
| Compare and hash | `diff`, `cmp`, `md5sum`, `sha256sum`, `b3sum` |
| Inspection | `jq`, `yq`, `xmllint` |
| Editors | `vim`, `neovim`, `nano` |

Editors that mmap (including some `code` configurations) are best-effort: they should not break, but they are not in the guaranteed tier.

## How the guarantee holds

Every projected file declares its size, byte source, read mode, and stability. The host wires those attributes into `st_size`, FUSE flags, direct-I/O behavior, and cache layers so the tool sees a file that behaves consistently. A tool that stat-sizes before reading, a tool that seeks, and a tool that reads in chunks all get coherent results because the projection layer, not the FUSE layer, is where these decisions are made.

The invariant also means that tool breakage counts as a bug regardless of which tool breaks it. If `tar` regresses while `cat` still works, the change that caused it is wrong.

## Where coverage is partial

Not every tool in the matrix is covered by automated CI today. The compatibility list is the contract the design holds itself to. For changes to provider path surfaces, traversal, or routing, validate by running `ll`, `cd`, and `find` from the provider root through every intermediate directory in a live container, and run the smoke harness in `tests/smoke/`. The automated tests and manual traversal together are the enforcement path while full coverage is being built out.
