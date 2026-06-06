# Cache architecture

Status: superseded

This note described the previous host cache (a durable browse/view tier with an in-memory canonical store layered beside it, plus blob and archive caches). That model has been **inverted and replaced**. See **[object-cache-primary.md](object-cache-primary.md)** for the current design:

- the **object cache** is the durable primary (canonical upstream bytes, keyed by anchor; `object.redb`);
- the **view cache** is derived and non-durable (`view.redb`, deleted on every startup), recomputed from the object cache with no upstream refetch;
- the **blob cache** (and archive-tree materialization) is unchanged;
- materialization (effects → cache writes) lives in the host; the `omnifs-cache` crate is pure byte storage.

The blob-cache and archive-tree sections of the old note (`fetch-blob`, `read-blob`, `open-archive`, `ExtractKey`, `TreeMaterializer`, publish-by-rename) still describe the current blob/archive behavior accurately and are folded into the successor doc's "Blob cache" coverage.
