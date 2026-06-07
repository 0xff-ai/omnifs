# omnifs-provider-db

A database provider for omnifs. v1 ships SQLite-only and exposes a
read-only browse surface; PostgreSQL and other backends will slot
behind the same path tree later.

## Design

SQLite runs **inside the WASM provider** through `rusqlite` with
the `bundled` feature. The host preopens the directory containing
the database file via Wasmtime's WASI `preopened_dir`; the provider
opens that file with `Connection::open_with_flags` using the URI
form `file:<path>?mode=ro&immutable=1`. No new WIT callouts. No
bytes cross the WIT boundary: SQLite pages on demand through its
VFS, which sits on top of WASI's `fd_read` / `fd_seek` / `path_open`.

The `?immutable=1` flag tells SQLite the file will not be modified
by any process, so it skips WAL / journal handling entirely. That
lets databases shipped as snapshots open without their `-wal` /
`-shm` sidecars. The cost: SQLite assumes the file truly does not
change. For a developer browsing a snapshot, that is acceptable;
if the file does change underneath, the read-only retry path
without `immutable=1` kicks in.

The provider opens **read-only by default**. The application-layer
`"read_only": false` escape hatch flips both the open flags and
the URI mode, but the host still needs `"mode": "rw"` on the
preopen for the kernel to allow writes through. Use it sparingly.

Table metadata and database info are **object-shaped**: one `load()`
per table or for the `/meta` singleton produces a provider-synthesized
canonical JSON document; warm field leaves (`schema.sql`, `schema.json`,
`indexes.json`, `count.txt`, `version.txt`, `path.txt`) render from
that canonical without re-querying SQLite. Object-cache invariant #3
(canonical byte-equals a single upstream GET) does **not** apply here:
the canonical is synthesized locally, not fetched verbatim from a remote.

Both objects declare compile-time **`Immutable`** stability (fixed snapshot
semantics with `read_only=true` / `immutable=1`). `sample.json` remains
route-shaped with a version token and ranged reads above the inline cap.

## Path tree

```
/db/meta/info.json                # canonical db.database object (pretty JSON + \n)
/db/meta/version.txt             # warm projection from info canonical
/db/meta/path.txt                # warm projection from info canonical
/db/tables/                      # exhaustive table name listing (no preload)
/db/tables/{table}/table.json    # canonical db.table object (pretty JSON + \n)
/db/tables/{table}/schema.sql    # warm projection from table canonical
/db/tables/{table}/schema.json   # warm projection from table canonical
/db/tables/{table}/indexes.json  # warm projection from table canonical
/db/tables/{table}/count.txt     # warm projection from table canonical
/db/tables/{table}/sample.json   # route-shaped: SELECT * LIMIT sample_limit (default 20)
```

Views and global `/indexes/` are deferred to a later surface. Per-row
directories (`rows/{pk}/...`) are out of scope: composite PKs,
non-integer PKs, and tables without a primary key all need design
work that has not happened.

## File attributes

Object leaves inherit **`Immutable`** from the `#[object(stability = Immutable)]`
declaration. `sample.json` is **`Mutable`** with a content hash version token;
large samples switch to a deferred ranged projection above the inline cap
(`MAX_PROJECTED_BYTES = 64 KiB`).

## Example config

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

`capabilities.preopened_paths` is the host capability. Each entry
maps an absolute host path to an absolute guest path. The host
validates that neither contains `..` segments before passing them
to Wasmtime. `mode: "ro"` uses `DirPerms::READ + FilePerms::READ`;
`mode: "rw"` uses `READ | MUTATE` for both.

## Swapping in your own database

Drop a SQLite file onto the host, mount it through the preopened
directory, and point `config.path` at the guest-side location.
The smoke harness uses Chinook:

```bash
mkdir -p providers/db/testdata
curl -sL -o providers/db/testdata/chinook.sqlite \
  https://github.com/lerocha/chinook-database/raw/master/ChinookDatabase/DataSources/Chinook_Sqlite.sqlite

# Contributor path: omnifs dev mounts the db provider with this fixture — see AGENTS.md
omnifs dev -y
docker exec omnifs /bin/zsh -lc 'ls /omnifs/db/tables'
docker exec omnifs /bin/zsh -lc 'cat /omnifs/db/tables/Album/table.json'
docker exec omnifs /bin/zsh -lc 'cat /omnifs/db/tables/Album/schema.sql'
```

## Build notes

The `bundled` feature compiles the C SQLite source against
`wasi-libc`. That needs a wasi-sysroot with headers, which the Rust
`wasm32-wasip2` toolchain does not ship. The repo's Dockerfile
downloads `wasi-sdk` and points `cc-rs` at it via:

```
WASI_SYSROOT=/opt/wasi-sdk/share/wasi-sysroot
CC_wasm32_wasip2=/opt/wasi-sdk/bin/clang
CFLAGS_wasm32_wasip2=--sysroot=/opt/wasi-sdk/share/wasi-sysroot
```

For local builds outside Docker, set the same variables before
calling `cargo build --target wasm32-wasip2`.

## What's deferred

- Views (`/views/...`) and global `/indexes/` directories.
- Per-row paths (`rows/{pk}/...`); needs design for composite,
  non-integer, and missing-PK tables.
- PostgreSQL backend (a network callout, plus a connection-pool
  story). The `database_type` discriminator is already in place.
- Write paths. Read-only is the only flow exercised; the `read_only:
  false` escape hatch exists but is mostly there for journal-mode
  quirks.
