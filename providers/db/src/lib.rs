#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

//! `omnifs-provider-db`: relational database provider for omnifs.
//!
//! Mirrors a database into a projected filesystem. Today this is a
//! `SQLite`-only build: `rusqlite` (with the `bundled` feature) compiles
//! the C `SQLite` source against `wasi-libc`, opens the database file
//! through preopened WASI directories, and exposes schema, indexes,
//! counts, and small samples per table. `PostgreSQL` or other backends
//! would slot in behind the same path tree with a new `database_type`
//! discriminator (likely as a network callout).
//!
//! The provider opens `SQLite` read-only by default with `mode=ro` and
//! `immutable=1` so databases left in WAL mode and shipped as
//! snapshots open without their `-wal` / `-shm` sidecars.

use std::cell::RefCell;
use std::rc::Rc;

use omnifs_sdk::prelude::Result;
use omnifs_sdk::serde::Deserialize;

mod meta;
mod provider;
mod sqlite_backend;
mod sqlite_backend_error;
mod table_subtree;
mod tables;

use sqlite_backend::SqliteBackend;

/// Database backend discriminator. `Sqlite` is the only variant
/// today; future backends slot in as additional arms.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(crate = "omnifs_sdk::serde", rename_all = "lowercase")]
pub(crate) enum DatabaseType {
    Sqlite,
}

#[derive(Clone)]
#[omnifs_sdk::config]
pub(crate) struct Config {
    /// Backend selector. Currently only `"sqlite"` is supported.
    #[serde(default = "default_database_type")]
    pub database_type: DatabaseType,
    /// Absolute path to the database file, as seen by the WASM
    /// component (i.e. through a preopened WASI directory).
    pub path: String,
    /// Open the database read-only. Defaults to true. The host
    /// preopen mode should match: read-only providers receive
    /// `DirPerms::READ + FilePerms::READ` preopens.
    #[serde(default = "default_read_only")]
    pub read_only: bool,
    /// Maximum rows returned in `_sample.json`. Defaults to 20.
    /// Tables with more rows are still counted in `_count.txt`,
    /// but `_sample.json` is truncated to `sample_limit`.
    #[serde(default = "default_sample_limit")]
    pub sample_limit: u32,
}

fn default_database_type() -> DatabaseType {
    DatabaseType::Sqlite
}

fn default_read_only() -> bool {
    true
}

fn default_sample_limit() -> u32 {
    20
}

/// Single-threaded provider state. The `rusqlite` `Connection` is
/// `!Send`, which fits the runtime's `Rc`-based model. Cached behind
/// `Rc<RefCell<..>>` so handlers can borrow it from inside
/// `Cx::state`. `SQLite` calls are synchronous, so each handler
/// holds the borrow for the duration of one query batch.
pub(crate) struct State {
    pub config: Config,
    pub backend: Rc<RefCell<SqliteBackend>>,
}
