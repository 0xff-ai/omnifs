#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
#![allow(clippy::needless_pass_by_value)]

//! `omnifs-provider-db`: relational database provider for omnifs.
//!
//! Mirrors a database into a projected filesystem. Today this is a
//! `SQLite`-only build: `rusqlite` (with the `bundled` feature) compiles
//! the C `SQLite` source against `wasi-libc`, opens the database file
//! through preopened WASI directories, and exposes schema, indexes,
//! counts, and small samples per table.
//!
//! The provider opens `SQLite` read-only by default with `mode=ro` and
//! `immutable=1` so databases left in WAL mode and shipped as
//! snapshots open without their `-wal` / `-shm` sidecars.

use hashbrown::HashSet;
use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;
use std::str::FromStr;

use omnifs_sdk::handler::DirIntent;
use omnifs_sdk::prelude::*;
use omnifs_sdk::serde::{Deserialize, Serialize};

mod backend;

use backend::{FileInfo, SqliteBackend};

#[derive(Clone)]
#[omnifs_sdk::config]
pub(crate) struct Config {
    /// Absolute path to the database file, as seen by the WASM
    /// component (i.e. through a preopened WASI directory).
    pub path: String,
    /// Open the database read-only. Defaults to true. The host
    /// preopen mode should match: read-only providers receive
    /// `DirPerms::READ + FilePerms::READ` preopens.
    #[serde(default = "default_read_only")]
    pub read_only: bool,
    /// Maximum rows returned in `sample.json`. Defaults to 20.
    /// Tables with more rows are still counted in `count.txt`,
    /// but `sample.json` is truncated to `sample_limit`.
    #[serde(default = "default_sample_limit")]
    pub sample_limit: u32,
}

fn default_read_only() -> bool {
    true
}

fn default_sample_limit() -> u32 {
    20
}

/// Single-threaded provider state. The `rusqlite` `Connection` is
/// `!Send`, which fits the runtime's `Rc`-based model. Stored behind
/// `Rc<RefCell<..>>` so handlers can borrow it from inside
/// `Cx::state`. `SQLite` calls are synchronous, so each handler
/// holds the borrow for the duration of one query batch.
pub(crate) struct State {
    pub config: Config,
    pub backend: Rc<RefCell<SqliteBackend>>,
}

thread_local! {
    static KNOWN_TABLES: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
}

fn install_known_tables(names: impl IntoIterator<Item = String>) {
    KNOWN_TABLES.with(|cell| {
        *cell.borrow_mut() = names.into_iter().collect();
    });
}

/// Capture for `/tables/{table}`. The SDK treats dynamic route anchors as
/// navigable once their captures parse, before a table leaf handler can prove
/// existence. DB tables are a local snapshot, so this parser admits only names
/// observed at provider start to preserve lookup misses for absent tables.
#[derive(Clone, Debug)]
struct TableName(String);

impl TableName {
    fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for TableName {
    type Err = ();

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        if s.is_empty() || s.contains(['\0', '/', '\\']) {
            return Err(());
        }
        let known = KNOWN_TABLES.with(|cell| cell.borrow().contains(s));
        if !known {
            return Err(());
        }
        Ok(Self(s.to_string()))
    }
}

impl fmt::Display for TableName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[omnifs_sdk::path_captures]
struct TableKey {
    table: TableName,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct TableDoc {
    pub name: String,
    pub create_sql: Option<String>,
    pub columns: serde_json::Value,
    pub indexes: serde_json::Value,
    pub row_count: i64,
}

#[omnifs_sdk::provider(metadata = "omnifs.provider.json")]
impl DbProvider {
    type Config = Config;
    type State = State;

    fn start(config: Config, r: &mut Router<State>) -> Result<State> {
        let backend = SqliteBackend::open(&config.path, config.read_only)
            .map_err(|e| ProviderError::internal(format!("open sqlite database: {e}")))?;
        // Seed the admissibility set before registering routes so that any
        // `TableName::from_str` called during route registration sees the full
        // table list.
        install_known_tables(
            backend
                .list_tables()
                .map_err(|e| ProviderError::internal(format!("list tables: {e}")))?,
        );
        let state = State {
            config,
            backend: Rc::new(RefCell::new(backend)),
        };

        r.dir("/meta").handler(meta_dir)?;
        r.file("/meta/info.json").handler(meta_info_json)?;
        r.file("/meta/version.txt").handler(meta_version)?;
        r.file("/meta/path.txt").handler(meta_path)?;

        // `/tables/{table}` is a keyed directory of direct file reads (live
        // SQLite, no canonical bytes), not an object. The parent `tables_list`
        // lists table names; the static file routes below list as its children.
        r.dir("/tables").handler(tables_list)?;
        r.file("/tables/{table}/table.json").handler(table_json)?;
        r.file("/tables/{table}/schema.sql")
            .handler(table_schema_sql)?;
        r.file("/tables/{table}/schema.json")
            .handler(table_schema_json)?;
        r.file("/tables/{table}/indexes.json")
            .handler(table_indexes_json)?;
        r.file("/tables/{table}/count.txt")
            .handler(table_count_txt)?;
        r.file("/tables/{table}/sample.json")
            .handler(table_sample)?;

        Ok(state)
    }
}

impl TableDoc {
    fn schema_sql(&self) -> FileProjection {
        text_content(self.create_sql.as_deref().unwrap_or("").as_bytes().to_vec())
    }

    fn schema_json(&self) -> Result<FileProjection> {
        let mut bytes = serde_json::to_vec_pretty(&self.columns)
            .map_err(|e| ProviderError::internal(format!("encode schema: {e}")))?;
        bytes.push(b'\n');
        Ok(FileProjection::body(bytes)
            .content_type(ContentType::Json)
            .build())
    }

    fn indexes_json(&self) -> Result<FileProjection> {
        let mut bytes = serde_json::to_vec_pretty(&self.indexes)
            .map_err(|e| ProviderError::internal(format!("encode indexes: {e}")))?;
        bytes.push(b'\n');
        Ok(FileProjection::body(bytes)
            .content_type(ContentType::Json)
            .build())
    }

    fn count(&self) -> FileProjection {
        text_content(format!("{}\n", self.row_count))
    }
}

async fn meta_dir(_cx: DirCx<State>) -> Result<DirProjection> {
    Ok(DirProjection::exhaustive([
        Entry::file("info.json"),
        Entry::file("version.txt"),
        Entry::file("path.txt"),
    ]))
}

fn read_file_info(cx: &Cx<State>) -> Result<FileInfo> {
    cx.state(|state| {
        state
            .backend
            .borrow()
            .file_info()
            .map_err(|e| ProviderError::internal(format!("file_info: {e}")))
    })
}

async fn meta_info_json(cx: Cx<State>) -> Result<FileProjection> {
    let info = read_file_info(&cx)?;
    let bytes = pretty_json(&info)?;
    Ok(FileProjection::body(bytes)
        .content_type(ContentType::Json)
        .dynamic()
        .build())
}

async fn meta_version(cx: Cx<State>) -> Result<FileProjection> {
    let info = read_file_info(&cx)?;
    FileInfo::version(&info)
}

async fn meta_path(cx: Cx<State>) -> Result<FileProjection> {
    let info = read_file_info(&cx)?;
    FileInfo::path(&info)
}

fn read_table_doc(cx: &Cx<State>, key: &TableKey) -> Result<TableDoc> {
    cx.state(|state| {
        let backend = state.backend.borrow();
        let name = key.table.as_str();
        if !backend
            .table_exists(name)
            .map_err(|e| ProviderError::internal(format!("table_exists: {e}")))?
        {
            return Err(ProviderError::not_found(format!("table not found: {name}")));
        }
        let create_sql = backend
            .table_create_sql(name)
            .map_err(|e| ProviderError::internal(format!("read schema: {e}")))?;
        let columns = backend
            .table_columns(name)
            .map_err(|e| ProviderError::internal(format!("columns: {e}")))?;
        let indexes = backend
            .table_indexes(name)
            .map_err(|e| ProviderError::internal(format!("indexes: {e}")))?;
        let row_count = backend
            .table_row_count(name)
            .map_err(|e| ProviderError::internal(format!("count: {e}")))?;
        Ok(TableDoc {
            name: name.to_string(),
            create_sql,
            columns: serde_json::to_value(&columns)
                .map_err(|e| ProviderError::internal(format!("encode columns: {e}")))?,
            indexes: serde_json::to_value(&indexes)
                .map_err(|e| ProviderError::internal(format!("encode indexes: {e}")))?,
            row_count,
        })
    })
}

async fn table_json(cx: Cx<State>, key: TableKey) -> Result<FileProjection> {
    let doc = read_table_doc(&cx, &key)?;
    let bytes = pretty_json(&doc)?;
    Ok(FileProjection::body(bytes)
        .content_type(ContentType::Json)
        .dynamic()
        .build())
}

async fn table_schema_sql(cx: Cx<State>, key: TableKey) -> Result<FileProjection> {
    let doc = read_table_doc(&cx, &key)?;
    Ok(doc.schema_sql())
}

async fn table_schema_json(cx: Cx<State>, key: TableKey) -> Result<FileProjection> {
    let doc = read_table_doc(&cx, &key)?;
    doc.schema_json()
}

async fn table_indexes_json(cx: Cx<State>, key: TableKey) -> Result<FileProjection> {
    let doc = read_table_doc(&cx, &key)?;
    doc.indexes_json()
}

async fn table_count_txt(cx: Cx<State>, key: TableKey) -> Result<FileProjection> {
    let doc = read_table_doc(&cx, &key)?;
    Ok(doc.count())
}

impl FileInfo {
    fn version(info: &Self) -> Result<FileProjection> {
        Ok(text_content(format!("{}\n", info.sqlite_version)))
    }

    fn path(info: &Self) -> Result<FileProjection> {
        Ok(text_content(format!("{}\n", info.path)))
    }
}

async fn tables_list(cx: DirCx<State>) -> Result<DirProjection> {
    match cx.intent() {
        DirIntent::Lookup { child } => {
            let exists = cx.state(|state| {
                state
                    .backend
                    .borrow()
                    .table_exists(child)
                    .map_err(|e| ProviderError::internal(format!("table_exists: {e}")))
            })?;
            if !exists {
                return Ok(DirProjection::exhaustive([]));
            }
            Ok(DirProjection::exhaustive([Entry::dir(child)]))
        },
        DirIntent::List { .. } | DirIntent::ReadFile { .. } => {
            let names = cx.state(|state| {
                state
                    .backend
                    .borrow()
                    .list_tables()
                    .map_err(|e| ProviderError::internal(format!("list tables: {e}")))
            })?;
            Ok(DirProjection::exhaustive(names.into_iter().map(Entry::dir)))
        },
    }
}

async fn table_sample(cx: Cx<State>, key: TableKey) -> Result<FileProjection> {
    let (bytes, version) = cx.state(|state| {
        let limit = state.config.sample_limit;
        let backend = state.backend.borrow();
        let name = key.table.as_str();
        if !backend
            .table_exists(name)
            .map_err(|e| ProviderError::internal(format!("table_exists: {e}")))?
        {
            return Err(ProviderError::not_found(format!("table not found: {name}")));
        }
        let rows = backend
            .table_sample(name, limit)
            .map_err(|e| ProviderError::internal(format!("sample: {e}")))?;
        let mut bytes = serde_json::to_vec_pretty(&rows)
            .map_err(|e| ProviderError::internal(format!("encode sample: {e}")))?;
        bytes.push(b'\n');
        let version = backend.table_version(name).ok();
        Ok((bytes, version))
    })?;

    // The sample is bounded by `sample_limit` and already fully in memory, so
    // serve it as a whole-file body projection: the host returns it through the
    // read-file terminal in one shot, with no inline-size cap (unlike a
    // dir-entry-embedded inline source) and without the ranged open/read-chunk
    // session a streaming source would need.
    let mut builder = FileProjection::body(bytes)
        .content_type(ContentType::Json)
        .dynamic();
    if let Some(v) = version {
        builder = builder.version(v);
    }
    Ok(builder.build())
}

fn text_content(bytes: impl Into<Vec<u8>>) -> FileProjection {
    FileProjection::body(bytes)
        .content_type(ContentType::Text)
        .build()
}
