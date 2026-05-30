---
title: Guides
description: Task-oriented how-to guides for omnifs — browse the filesystem, work with each provider, authenticate, manage mounts, and debug the runtime.
---

These guides are short, task-oriented recipes: "to do X, run Y." Each one is
independent and skimmable — start with whichever matches what you are trying to
do. If you are new, work through [Getting started](/getting-started/quickstart/)
first; the guides assume you already have a mount up and a shell open inside the
container.

## Using the filesystem

- [Browsing the filesystem](/guides/browsing/) — `cd`, `ls`, `cat`, `grep`,
  `find`; what behaves like a real file and the bash-tool compatibility contract.
- [Using omnifs with agents](/guides/agents/) — paths as a stable tool surface;
  read-the-file instead of calling an API.

## Working with providers

- [Working with GitHub](/guides/github/) — owners, repos, issues, PRs, diffs, CI
  runs, and clone-on-list.
- [Querying DNS](/guides/dns/) — records by type, resolver selection, and reverse
  lookups.
- [Browsing arXiv](/guides/arxiv/) — papers by id or category scan; PDFs, source,
  metadata, and versions.

See the [Provider catalog](/providers/) for the full per-provider path reference.

## Configuration and credentials

- [Authenticating providers](/guides/authentication/) — OAuth login, importing
  existing tokens, and checking credential status.
- [Managing mounts](/guides/managing-mounts/) — add, list, and remove mounts, and
  where their configs live.

## Running and debugging

- [Container lifecycle](/guides/container-lifecycle/) — `up`, `down`, `shell`,
  and `logs`.
- [Inspect & debug](/guides/inspect-debug/) — `status`, `doctor`, `logs`, and the
  live `inspect` event stream.
- [Troubleshooting](/guides/troubleshooting/) — SSH agent issues,
  `Input/output error` on repo paths, and mount-missing checks.
