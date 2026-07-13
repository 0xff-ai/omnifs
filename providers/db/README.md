# omnifs-provider-db

A read-only SQLite provider for omnifs.

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

The provider opens **read-only**. The host grants only a read-only preopen, so
the sandbox cannot write through the SQLite connection.

Table metadata and database info are **path-shaped**. The SQLite file is
already the local source of truth, so this provider does not emit canonical
object-cache entries for `/meta` or `/tables/{table}`. Each metadata leaf reads
directly from the backend and returns file bytes for that path.

The table name universe is captured at provider start so missing tables do not
appear as synthetic dynamic route anchors. `sample.json` remains route-shaped
with a version token and ranged reads above the inline cap.

## Path tree

```
/db/meta/info.json                # direct database info JSON (pretty JSON + \n)
/db/meta/version.txt             # direct SQLite version text
/db/meta/path.txt                # direct database path text
/db/tables/                      # exhaustive table name listing (no preload)
/db/tables/{table}/table.json    # direct table metadata JSON (pretty JSON + \n)
/db/tables/{table}/schema.sql    # direct CREATE TABLE SQL
/db/tables/{table}/schema.json   # direct column metadata JSON
/db/tables/{table}/indexes.json  # direct index metadata JSON
/db/tables/{table}/count.txt     # direct row count text
/db/tables/{table}/sample.json   # route-shaped: SELECT * LIMIT sample_limit (default 20)
```

## File attributes

Metadata leaves are direct read projections. `sample.json` is a **`Dynamic`**
ranged projection (the route is declared `ranged`) with a content hash version
token, so a sample of any size is served through one ranged session.

## Setup

Run `omnifs mount add db` and enter the host path to the SQLite file. The
provider-owned config is:

```json
{
  "path": "/data/test.db",
  "read_only": true,
  "sample_limit": 20
}
```

The generated mount spec inherits the provider's memory limit and dynamic
preopen grant. At mount start the host resolves `path` as a read-only WASI
preopen; the provider never receives general host filesystem access.

## Swapping in your own database

Drop a SQLite file onto the host and point `config.path` at it.
The smoke harness uses Chinook:

```bash
mkdir -p providers/db/testdata
curl -sL -o providers/db/testdata/chinook.sqlite \
  https://github.com/lerocha/chinook-database/raw/master/ChinookDatabase/DataSources/Chinook_Sqlite.sqlite

# Contributor path: just dev mounts the db provider with this fixture.
just dev -y
# In the shell opened at /omnifs:
ls /omnifs/db/tables
cat /omnifs/db/tables/Album/table.json
cat /omnifs/db/tables/Album/schema.sql
```

## Build notes

The `bundled` feature compiles SQLite against `wasi-libc`, which needs the
wasi-sdk sysroot absent from the Rust `wasm32-wasip2` toolchain. Use `just
providers build`; it installs the pinned wasi-sdk and supplies the compiler and
sysroot settings.

## Unprojected surfaces

- Views (`/views/...`) and global `/indexes/` directories.
- Per-row paths (`rows/{pk}/...`); needs design for composite,
  non-integer, and missing-PK tables.
- PostgreSQL backend (a network callout, plus a connection-pool
  story).
- Write paths.
