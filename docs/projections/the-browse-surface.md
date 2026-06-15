---
title: The browse surface
description: The three operations a reader's open, readdir, and read become at the host–provider boundary.
---

A reader's `ls` and `cat` become three operations at the host–provider boundary: look up a single named child (`lookup-child`), list a directory's children (`list-children`), and read a file's bytes (`read-file`). Large or streamed content adds `open-file`, `read-chunk`, and `close-file`. That is the whole namespace contract a frontend needs.

## Lookup and listings

`lookup-child` is the authoritative name oracle. A directory listing may be non-exhaustive: `list-children` reports the names omnifs is aware of, not necessarily every name that exists. Calling `lookup-child` for a specific name can resolve it even when that name did not appear in the latest listing. A provider uses this to offer an effectively infinite namespace — every issue number or every DNS record — without enumerating it upfront.

When a provider route has a capture sibling at the next depth, the listing for the parent directory is marked non-exhaustive. Exhaustive listings are only correct when the provider can name all children.

## Reading files

`read-file` returns file bytes. For small or stable content the provider returns a full payload. For large or volatile content it opens a ranged session through `open-file`, `read-chunk`, and `close-file`.

The `cached-canonical` parameter on `read-file` is the host's way of pushing already-stored object bytes back into the provider for rendering. If the host has a valid cached canonical for the object behind this path, the provider can render without a round-trip to the upstream service.

## Names

Directories and files work like normal paths. The provider registers routes with literal segments and capture segments. Literal segments auto-navigate: any intermediate directory in a registered route path answers `lookup-child` and `list-children` without a stub handler. Capture segments participate in candidacy: if a segment value fails to parse as the capture type, the host tries the next-most-specific route rather than returning ENOENT immediately.

## What is not here

Callouts, effects, lifecycle, and continuation (`resume`, `cancel`) are not part of the browse surface. They are the mechanism the engine uses to run provider work and commit results. That machinery is covered under the Engine section.
