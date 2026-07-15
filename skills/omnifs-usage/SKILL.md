---
name: omnifs-usage
description: Navigate an omnifs frontend as a read-only projected filesystem. Use when exploring mounted providers, reading route README leaves, handling paginated directories, or using shell tools against a host location or the guest /omnifs path.
---

# Navigating omnifs mounts

An omnifs frontend is a read-only filesystem projection of external services. Treat its location like a normal directory tree, but discover it incrementally: list first, read the generated `README.md` route schema where available, then follow concrete paths. Docker and libkrun expose the tree at `/omnifs`; a host frontend uses the absolute location chosen when it was enabled.

## Ground rules

1. Start with `ls`, not recursive traversal. Provider listings can be partial, paged, or backed by upstream calls.
2. Read `README.md` explicitly at the provider root or a top-level branch to understand route templates, captures, finite choices, and examples.
3. Use lookup naturally. A path can exist even when the latest `ls` did not enumerate it, especially when a route has captures like `{owner}` or `{repo}`.
4. Treat the mount as read-only. Do not create, edit, delete, rename, chmod, or move files inside it.
5. Prefer narrow commands from the directory you are inspecting. Avoid broad commands from the mount root.

## Discovery loop

1. Set `ROOT` to the frontend location, such as `/omnifs` in a guest or the host mount path.
2. `ls "$ROOT"` to see configured mounts.
3. `ls "$ROOT/<mount>"` to see provider roots and `README.md`.
4. `cat "$ROOT/<mount>/README.md"` to read the generated route schema.
5. Substitute concrete values for captures in the route templates.
6. List each intermediate directory before reading leaves.

## Pagination

Some directories expose pagination controls:

- `@next` loads one more page into the current directory listing.
- `@all` drains remaining pages, subject to host safety caps.
- Read controls with `cat`, then run `ls` again to see the expanded listing.
- Ignore-respecting recursive tools should skip these controls by default.

## Freshness

Projected data can be dynamic. A file read or directory listing may call upstream, serve cached bytes, or use a validator. If a result looks stale, re-read the specific path or list the specific parent directory again. Do not assume a recursive scan refreshes the whole mount.

With `omnifs up --offline`, the daemon serves only complete durable cache facts. A path that needs upstream work returns an offline miss rather than silently becoming `NotFound`; switch back to ordinary `omnifs up` when the missing data must be fetched.

## Do not

- Do not run `find /omnifs` or `grep -r` from the mount root as a first move.
- Do not write into the mount.
- Do not assume every directory listing is exhaustive.
- Do not treat provider paths as local cache files.
- Do not bypass `README.md` when a route template is unclear.

## Worked one-liners

```bash
ls /omnifs
```

```bash
cat /omnifs/github/README.md
```

```bash
ls /omnifs/github/octocat/Hello-World/issues/open
```

```bash
cat /omnifs/github/octocat/Hello-World/issues/open/7/item.md
```

```bash
cat /omnifs/github/octocat/Hello-World/issues/open/@next && ls /omnifs/github/octocat/Hello-World/issues/open
```
