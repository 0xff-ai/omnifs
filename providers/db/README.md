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

## Path tree (v1)

```
/db/meta/version.txt              # rusqlite::version()
/db/meta/path.txt                 # the configured file path
/db/meta/info.json                # size, page_size, page_count, app_id, user_version, journal_mode
/db/tables/                       # one directory per user table
/db/tables/{table}/schema.sql    # CREATE TABLE statement from sqlite_master
/db/tables/{table}/schema.json   # columns from PRAGMA table_info
/db/tables/{table}/indexes.json  # PRAGMA index_list + index_info
/db/tables/{table}/count.txt     # SELECT count(*)
/db/tables/{table}/sample.json   # SELECT * LIMIT sample_limit (default 20)
```

Views and global `/indexes/` are deferred to a v2 surface. Per-row
directories (`_rows/{pk}/...`) are out of scope: composite PKs,
non-integer PKs, and tables without a primary key all need design
work that has not happened.

## File attributes

Every projected file declares `Mutable + Inline` with a content
hash as the version token, except `_sample.json` for large samples,
which switches to a deferred ranged projection above the inline
cap (`MAX_PROJECTED_BYTES = 64 KiB`). The host keys cache entries
by the version token, so the same projection is served from cache
until the version moves.

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
- Per-row paths (`_rows/{pk}/...`); needs design for composite,
  non-integer, and missing-PK tables.
- PostgreSQL backend (a network callout, plus a connection-pool
  story). The `database_type` discriminator is already in place.
- Write paths. Read-only is the only flow exercised; the `read_only:
  false` escape hatch exists but is mostly there for journal-mode
  quirks.
