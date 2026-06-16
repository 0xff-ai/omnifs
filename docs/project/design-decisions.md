# Design decisions

The reasoning behind the load-bearing choices, in decision-record form. Each is stated as the decision, the forces, and the trade-off accepted.

Why filesystem paths. Paths are the one interface with backward compatibility to every existing tool and forward compatibility to every agent trained on shell transcripts. The trade-off: a path namespace is a weaker query surface than SQL, which is why query-shaped routes and a future fast-path hook matter.

Why providers ask and the host acts. Separating meaning (the provider) from trust (the host) is what makes a long tail of third-party providers safe to run. The trade-off: every external effect is a callout round trip, so providers cannot do ambient I/O even when it would be convenient.

Why object and view caches are separate. Canonical upstream bytes are the durable asset, and rendered views are derived and disposable. Keeping them apart lets the view cache rebuild from canonical with no refetch. The trade-off: providers must declare identity and stability accurately for the split to hold.

Why WIT and WASM. A `wasm32-wasip2` component with capability-based imports is the first credible sandbox for untrusted integration code at near-native speed. The trade-off: the WIT contract is flag-day breaking until a versioning story exists, so contract changes are batched and called out.

Why the FUSE and container boundary. Linux FUSE is the runtime mount, and the container makes macOS and Windows practical without claiming a native mount there. The trade-off: macOS and Windows users read through the container shell today.

Write-flow direction. Reads stay read-only. Writes, when they land, stage intent as reviewable diffs rather than implicit file writes. The trade-off: no naive write-through, ever, even though it would be the obvious first thing to build.
