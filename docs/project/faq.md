# FAQ

This page answers the comparative questions once, so they are not re-argued elsewhere.

How is this different from MCP? They compose rather than compete. MCP is how an agent acts; omnifs is how it sees. You can mount MCP servers as providers and serve the omnifs tree over MCP.

How is this different from an embedded agent workspace? An in-process virtual filesystem serves one application through reimplemented commands. A real mount serves every process on the machine at once, including ones that never heard of omnifs, so `vim`, `rsync`, and CI all work unmodified. That universality is the structural difference.

How is this different from rclone or a sync tool? Projection is lazy and on demand. Sync copies ahead of need and struggles with scale and staleness. omnifs materializes exactly the slice you touch, when you touch it.

Is it offline? Warm cached reads work without a new upstream call. Full offline snapshots beyond the warm cache are roadmap, so do not assume offline mode today.

Does it save tokens? It hands an agent a path instead of a re-described service, which is less context to carry. The measured figure ships with the public benchmark and its reproduce command, not before. Until then this is reduced context, not a number.

Can I write through it? Not yet. The read model is read-only. Writes, when they arrive, are explicit, reviewable diffs.

Does it run native on macOS? No. macOS and Windows run the mount in a Linux container, and you read through `omnifs shell`. Native non-Linux mounts are roadmap.
