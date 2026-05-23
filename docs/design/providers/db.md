# db provider

Status: v1 shipped (sqlite, read-only, browse-only); future surface designed below
Scope: `providers/db/`, `crates/host/src/config` (preopened paths capability), `crates/host/src/runtime/instance.rs` (sync wasmtime isolation)
Supersedes the postgres-specific draft: parts of `docs/design/providers/postgres.md` (the `pg-query` callout idea) are deferred behind the file-backed sqlite path, which works through standard WASI filesystem syscalls and needs no WIT extension.

## Summary

A generic database provider that mounts a single database (today: a SQLite file; tomorrow: Postgres, DuckDB, others) as a navigable filesystem. The user specifies the database type and connection in instance config; the same path layout applies regardless of backend. v1 ships SQLite-only, read-only, browse-only (schema + counts + a fixed sample); the backend abstraction is structured so adding Postgres later means adding a new backend module without touching the path layout.

The implementation runs SQLite **inside the WASM provider sandbox** using `rusqlite` with the `bundled` feature compiled against the wasi-sdk sysroot. The host preopens the database file's parent directory through Wasmtime's WASI ctx with read-only permissions; SQLite's VFS pages on demand through standard WASI `fd_read` / `fd_seek` syscalls. No new WIT callouts, no bytes crossing the WIT boundary, no extra host runtime layer to maintain.

## Status

What v1 ships:

- `/meta/version.txt`, `/meta/path.txt`, `/meta/info.json`
- `/tables/` (lists tables from `sqlite_master`)
- `/tables/{name}/schema.sql`
- `/tables/{name}/schema.json`
- `/tables/{name}/indexes.json`
- `/tables/{name}/count.txt`
- `/tables/{name}/sample.json`

What v1 does not ship and the document below covers as future work:

- Per-row paths (`/tables/{name}/rows/{pk}/...`)
- Saved queries (`/queries/{label}.json`)
- Views and global indexes
- Postgres backend
- Any kind of write surface (mutations are reserved for the git-via-mutation model)

## Path tree

```
/db/                                  (mount root; auto-nav)
/db/meta/                             (auto-nav)
/db/meta/version.txt                  (rusqlite library version)
/db/meta/path.txt                     (configured file path)
/db/meta/info.json                    (size, page_size, page_count, app_id,
                                       user_version, journal_mode)
/db/tables/                           (lists every table from sqlite_master)
/db/tables/{name}/                    (#[bind] → TableSubtree, name validated
                                       as a safe path segment)
  ├ /db/tables/{name}/schema.sql      (CREATE TABLE statement)
  ├ /db/tables/{name}/schema.json     (PRAGMA table_info: columns, types, PK)
  ├ /db/tables/{name}/indexes.json    (PRAGMA index_list + index_info)
  ├ /db/tables/{name}/count.txt       (SELECT count(*))
  └ /db/tables/{name}/sample.json     (SELECT * LIMIT sample_limit)
```

Names: top-level entries (`meta`, `tables`) have no underscore prefix. The underscore convention is used by providers (GitHub, DNS) where path segments compete with user-supplied names (`octocat` vs `_repo`); in a database, every top-level path is provider-managed and there is no collision risk. Removing underscores makes paths visually quieter and `cat /db/meta/version.txt` reads naturally.

Table names collide with magic segments only inside `tables/`. A table named `meta` lives at `/db/tables/meta/`; that does not collide with `/db/meta/`. A table named `tables` lives at `/db/tables/tables/`; that does not collide with `/db/tables/`. Safe.

## Configuration

```json
{
  "provider": "omnifs_provider_db.wasm",
  "mount": "db",
  "capabilities": {
    "max_memory_mb": 128,
    "preopened_paths": [
      { "host": "/data", "guest": "/data", "mode": "ro" }
    ]
  },
  "config": {
    "database_type": "sqlite",
    "path": "/data/test.db",
    "read_only": true,
    "sample_limit": 20
  }
}
```

`preopened_paths` is a new host capability shipped with this provider. Each entry tells the host to make `host` available inside the WASM sandbox as `guest`, with `mode: "ro" | "rw"`. The host validates that paths are absolute and contain no `..`. v1 only uses `ro`; `rw` is reserved for future write-capable backends.

`config.path` must be inside (or equal to) some preopened guest path. The provider opens the file by that guest-visible path.

`config.read_only` defaults to true. When true the provider opens SQLite with `SQLITE_OPEN_READ_ONLY` + `?mode=ro&immutable=1`; the URI's `immutable=1` skips all journal / WAL handling, which is correct for the v1 read-only mount. When false (escape hatch only), the provider opens read-write; the use case is recovery of WAL-mode databases that need a writable journal sidecar.

`config.sample_limit` caps `sample.json` row count; default 20.

`database_type` is the backend discriminator. v1 only accepts `"sqlite"`. Future values include `"postgres"`, `"duckdb"`, etc.; each backend module is internal to the provider crate.

## Capabilities and the sync-wasmtime isolation gotcha

The host's `CapabilitiesConfig` grows a `preopened_paths: Option<Vec<PreopenedPath>>` field. The runtime wires each entry into Wasmtime's `WasiCtxBuilder::preopened_dir(host, guest, DirPerms, FilePerms)` with read or read-write permission per the `mode` field.

A second host change ships with v1: `crates/host/src/runtime/instance.rs` wraps each sync wasmtime call in `std::thread::scope` so the call runs on a fresh OS thread with no tokio handle. This is the "load-bearing escape hatch" the agent's report flagged. Background:

- The host uses `wasmtime::add_to_linker_sync` and drives wasmtime from inside `tokio::Runtime::block_on(...)` on a tokio worker thread.
- The first time a guest calls a WASI filesystem syscall (i.e. the first read from SQLite), `wasmtime_wasi`'s internal `in_tokio` shim tries to use the tokio runtime handle in a way that conflicts with the already-running runtime and panics with "Cannot start a runtime from within a runtime."
- Running the wasmtime call on a thread without a tokio handle lets the WASI shim fall back to its private runtime singleton.

The right long-term fix is `wasmtime_wasi::add_to_linker_async` (switches the host to fiber-stacked async wasmtime calls; aligns with the rest of the host's async runtime). That is tracked as a follow-up; the per-op thread spawn is fine for current latency budgets but should not stay forever.

## File attributes

| File | Size | Bytes | Stability | Version |
|---|---|---|---|---|
| `/meta/version.txt` | Exact | Inline | Mutable | hash of (sqlite_version) |
| `/meta/path.txt` | Exact | Inline | Mutable | hash of configured path |
| `/meta/info.json` | Exact | Inline | Mutable | hash of (page_count, journal_mode, app_id, user_version) |
| `/tables/{name}/schema.sql` | Exact | Inline | Mutable | hash of `sqlite_master.sql` row |
| `/tables/{name}/schema.json` | Exact | Inline | Mutable | hash of column metadata |
| `/tables/{name}/indexes.json` | Exact | Inline | Mutable | hash of index metadata |
| `/tables/{name}/count.txt` | Exact | Inline | Mutable | no version (cheap to recompute) |
| `/tables/{name}/sample.json` | Exact | Inline/Deferred per size | Mutable | hash of (column set, current count, max rowid) |

`Mutable` reflects that SQL operations against the database can change these values; v1 mounts are read-only at the FUSE layer, so "mutable" here describes the upstream behavior, not the user's ability to write. The host caches by `(provider, path, version)` so a `cat` after `ls` is free until the underlying data actually changes.

Inline byte cap: 64 KiB per file, 512 KiB aggregate per response. `sample.json` for wide tables can exceed the per-file cap and gets `Deferred + Full` automatically; the table-subtree handler materializes the bytes on read.

## Listing semantics

`/tables/` enumerates exhaustively from `sqlite_master where type='table' and name not like 'sqlite_%'`. The internal `sqlite_` tables are hidden; `sqlite_sequence` and `sqlite_stat1` are visible only through PRAGMAs, not as user tables.

Each table directory's children are static (`schema.sql`, `schema.json`, `count.txt`, `sample.json`, `indexes.json`); the listing is exhaustive.

`/meta/` is exhaustive over the three files above.

The mount root (`/`) is auto-navigable from the registered literal-segment prefixes; `meta/` and `tables/` are the only entries.

## Backend abstraction

Internally the provider has a `sqlite_backend` module that owns:

- The `rusqlite::Connection` (single-threaded, behind `Rc<RefCell<...>>` per the SDK's runtime model).
- Connection lifecycle (opens once at mount load with the URI form `file:{path}?mode=ro&immutable=1`).
- Query helpers (`columns(table)`, `indexes(table)`, `count(table)`, `sample(table, limit)`).
- Error translation (`SqliteBackendError` → `ProviderError::not_found / invalid_input / internal`).

The path handlers in `meta.rs`, `tables.rs`, and `table_subtree.rs` call into the backend through a small trait so adding Postgres later is "implement the trait against a network connection". The provider's lib.rs picks the backend at init based on `database_type`.

## Future shape

This is the documented design space the next iterations of the provider can fill in. v1 deliberately stays browse-only; the directions below are the natural extensions.

### Per-row paths

The biggest gap: today there is no way to address an individual row. The shape:

```
/db/tables/{table}/rows/{pk}/             (#[bind] → RowSubtree)
/db/tables/{table}/rows/{pk}/row.json     (full row as JSON)
/db/tables/{table}/rows/{pk}/{column}     (one file per column)
```

Open questions before this lands:

1. **PK encoding in the path segment.** Single-column integer PKs are easy (`rows/42/`). Single-column text PKs need percent-encoding for `/` and other path-hostile characters. Composite PKs need a serialization; the postgres draft proposed comma-separated, percent-encoded segments (`rows/2024-01-01,raul%40example.com/`). The serialization is a route-parser function; once written, it is the same one the host uses on lookup and the provider uses on render.

2. **Row file shape.** Three options:
   - `row.json` only: one fetch returns the full record as JSON. Heavier reads when the user wanted one field.
   - Per-column files only: matches the GitHub provider's issue shape (`title`, `state`, ...). Lighter reads at the cost of one PRAGMA per row dir at materialization.
   - Both, with per-column files projected from the same row fetch via `proj_file`. The likely answer; matches existing provider conventions.

3. **Listing semantics for `rows/`.** A million-row table cannot enumerate every PK. The right answer leans on routing rule D4 (negative `lookup_child` is authoritative only when no capture sibling could match): `rows/` lists a sample (first N PKs) with `exhaustive: false`, and a lookup of any PK falls through to the parent `#[dir]` handler which performs a `SELECT * WHERE pk = ?` lookup per requested PK. The user sees a partial directory listing but `cat /db/tables/Album/rows/42/title` works for any 42 that exists.

4. **No-PK tables.** SQLite exposes an internal `rowid` for tables without an explicit PK. Using it as the PK is a footgun because `VACUUM` can renumber rowids. Two viable answers:
   - Refuse `rows/` for no-PK tables; only `sample.json` works. Conservative; the path simply does not exist.
   - Expose `rows/{rowid}/` with a loud warning in the table's `schema.json` ("rowid is unstable across VACUUM") so the user opts in deliberately.
   The likely answer is the first.

5. **Composite PK ordering.** SQLite's `PRAGMA table_info(table)` returns columns with their position in the PK; the encoding must use that order, not the column declaration order, so two equivalent PKs serialize to the same path.

The first slice to ship: single-column PK lookup, single `row.json` per row, sample-listed `rows/`. That covers most real-world tables (which have single integer PKs), avoids the column-file design fight, and matches the existing `sample.json` precedent. Subsequent PRs add per-column files, composite PK encoding, and no-PK refusal.

### Saved queries

```
/db/queries/{label}.json
```

`queries` is an instance-config map from label to SQL string:

```json
{
  "config": {
    "queries": {
      "active_users": "SELECT id, email FROM users WHERE last_seen > date('now', '-7 days')",
      "top_albums": "SELECT a.title, count(t.id) AS tracks FROM Album a JOIN Track t ON t.AlbumId = a.AlbumId GROUP BY a.AlbumId ORDER BY tracks DESC LIMIT 10"
    }
  }
}
```

Each label materializes as a single file at `/db/queries/{label}.json`. Reading the file executes the query and returns the result set as a JSON array of objects (column names as keys). Files use `Stability::Mutable` with no version token; every read re-executes.

Open questions:

1. **Parameter binding.** Saved queries with parameters (`SELECT * FROM users WHERE id = ?`) need a way to supply values. Options:
   - Path captures: `/db/queries/user_by_id/{id}.json`. Clean. Requires the config to declare which parameters are path-bound.
   - Query strings in the path: rejected (filesystems are not URL paths).
   - Multiple labels per parameterized template: `users_by_id_42` declared once per known value. Boring but works for small parameter sets.
2. **Result size limits.** A query returning 100k rows should not crash the provider. Cap result rows at `query_limit` (config-driven, default 1000); exceeding the cap returns the cap with a `truncated: true` field.
3. **Free-form SQL.** A user-supplied SQL endpoint (`echo "SELECT ..." > /db/exec.sql && cat /db/result.json`) is tempting but wrong: read-only mounts have no write surface for arbitrary input, the user-experience is awkward, and it's a security footgun (a misconfigured mount could be a SQL injection surface). The saved-query model is the explicit boundary: only configured queries run.

### Views and global indexes

```
/db/views/                              (lists views from sqlite_master where type='view')
/db/views/{view}/schema.sql             (CREATE VIEW statement)
/db/views/{view}/schema.json            (column metadata)
/db/views/{view}/sample.json            (SELECT * FROM view LIMIT sample_limit)
/db/indexes/                            (lists all indexes globally)
/db/indexes/{name}                      (CREATE INDEX statement)
```

Lower priority than per-row and saved queries because views can be modeled by saved queries, and per-table `indexes.json` already covers the common case.

### Postgres backend

Adding `database_type: "postgres"` is the next backend after sqlite. Key differences:

- Postgres speaks its own binary wire protocol over TCP or unix socket; the WASM provider cannot speak it directly.
- The likely shape: a new WIT callout arm `db-query(connection-id, sql, params) -> rows`. The host opens the Postgres connection (using a well-vetted library like `tokio-postgres`) and the provider sends SQL through the callout.
- The path tree above maps 1:1: `meta/`, `tables/{schema.table}/...`, `rows/{pk}/...`, `queries/{label}.json`. Postgres-specific extras (`pg_stat_user_tables`, `EXPLAIN`, `pg_indexes`) slot under `meta/` and per-table.
- Connection pooling, prepared statement caching, transaction boundaries: host-side concerns.

The detailed Postgres design lives in `docs/design/providers/postgres.md`; this provider's backend abstraction is shaped to drop that work in cleanly when the WIT extension is on the table.

## Bash-tool walkthrough

```bash
# what database is mounted and what does it contain
cat /db/meta/version.txt              # 3.49.2
cat /db/meta/info.json | jq           # size, page count, journal mode
ls /db/tables                         # Album Artist Customer Employee Genre ...

# explore a table
cat /db/tables/Album/schema.sql       # CREATE TABLE [Album] ...
cat /db/tables/Album/schema.json | jq # columns/types/nullability
cat /db/tables/Album/count.txt        # 347
cat /db/tables/Album/sample.json | jq '.[0]'

# standard tools work
find /db/tables -name schema.sql | head
grep -r INTEGER /db/tables/
wc -l /db/tables/Album/schema.sql
md5sum /db/tables/Album/sample.json
```

What does not work and why:

- `tail -f` on any file: nothing is volatile; live tailing has no source.
- Writes (`echo > ...`, `rm`, `mv`): mount is read-only.
- Per-row lookup (`cat /db/tables/Album/rows/42/title`): not yet implemented (see "Future shape").
- `find /db/tables/Album/rows -type d`: same; no `rows/` directory yet.
- `grep` across a large table's data: only `sample.json` is enumerated; the rest of the rows are not files yet.

## Mutation hooks (future)

Mutations follow the git-via-mutation model (`design/mutations-via-git.md`). Natural granularity for the db provider:

- Edit a row by writing to `rows/{pk}/{column}` or `rows/{pk}/row.json`. Commit. `git push` translates the diff into `UPDATE table SET column = ? WHERE pk = ?`.
- Create a new row by creating a new `rows/{new-pk}/` directory and committing. `INSERT INTO table ...`.
- Delete a row by `git rm -r rows/{pk}/`. `DELETE FROM table WHERE pk = ?`.
- DDL changes (`schema.sql` edits) explicitly out of scope; column-add/drop is too easy to get wrong through filesystem semantics.

This is documented for design coherence only; the read-only mount and mutation model are independent work streams.

## Open questions

1. **WIT addition for sqlite query offloading?** Today rusqlite runs entirely inside the WASM sandbox. For very large databases (multi-GB), this still works (pages on demand) but every operation incurs a wasmtime trip. Worth measuring; if a synthetic 10 GB benchmark shows tolerable latency, leave it. If not, the same `db-query` WIT extension we'd add for Postgres covers sqlite too (host opens with native rusqlite, provider sends SQL via callout).
2. **DatabaseType derive ordering.** The agent's report notes the SDK's `#[omnifs_sdk::config]` macro appends derives after user attributes, which triggers a "derive helper attribute used before introduced" lint when combined with `#[serde(rename_all = ...)]`. Track as a small SDK fix (insert derives at the front) so future config enums don't need the workaround.
3. **Multiple databases per mount.** A user might want `/db` to expose a directory of database files at `/db/databases/{name}/...`. Out of scope for v1 (one mount, one database); revisit when there is real demand.
4. **Sample ordering.** `sample.json` is currently `SELECT * LIMIT n` with no `ORDER BY`. SQLite returns rows in physical order, which is usually rowid-ascending. Deterministic enough for browsing; document the contract.
5. **Schema change invalidation.** A DDL change (`ALTER TABLE`) bumps `schema.sql`'s content hash, but the host caches by `(provider, path, version)`. The version derives from `sqlite_master` rows, which DDL updates, so this is correct. Worth a smoke test.
6. **Read-write escape hatch test.** `read_only: false` is the documented escape hatch for WAL-mode recovery. There is no test exercising it. Add one when we hit a database that needs it.
7. **Async wasmtime migration.** Tracked as the `add_to_linker_async` follow-up. Replaces the per-op `thread::scope` workaround in `instance.rs` with proper fiber-stacked async wasmtime calls.

## References

- `docs/design/providers/_context.md` — shared provider authoring briefing.
- `docs/design/providers/postgres.md` — the original generic-database design, parts of which (the `pg-query` callout) are deferred until Postgres lands.
- `docs/design/path-dispatch-and-listing.md` — routing precedence, listing exhaustiveness, lookup-vs-readdir authority split. D4 is the load-bearing rule for the future `rows/` lookup-fallthrough.
- `docs/design/file-attributes.md` — `Size` / `Bytes` / `Stability` / `VersionToken` contract.
- `docs/design/wasm-sandbox-substrate.md` — wasm32-wasip2 sandbox model.
- `providers/db/README.md` — operator-facing setup, configuration, fixture instructions.
- Chinook database (test fixture): https://github.com/lerocha/chinook-database
- `rusqlite` crate: https://docs.rs/rusqlite/
- `wasi-sdk`: https://github.com/WebAssembly/wasi-sdk (used in the Dockerfile to provide the WASI sysroot for rusqlite-bundled).
