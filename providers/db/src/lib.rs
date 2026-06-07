#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
#![allow(clippy::needless_pass_by_value)]

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
use std::collections::HashSet;
use std::fmt;
use std::rc::Rc;
use std::str::FromStr;

use omnifs_sdk::browse::FileContent;
use omnifs_sdk::handler::DirIntent;
use omnifs_sdk::prelude::*;
use omnifs_sdk::serde::{Deserialize, Serialize};

mod backend;

use backend::{FileInfo, SqliteBackend};

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
    /// Maximum rows returned in `sample.json`. Defaults to 20.
    /// Tables with more rows are still counted in `count.txt`,
    /// but `sample.json` is truncated to `sample_limit`.
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

thread_local! {
    static KNOWN_TABLES: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
}

fn install_known_tables(names: impl IntoIterator<Item = String>) {
    KNOWN_TABLES.with(|cell| {
        *cell.borrow_mut() = names.into_iter().collect();
    });
}

/// Capture for `/tables/{table}`. Validates the name is non-empty, path-safe,
/// and present in the database snapshot opened at provider start. A parse
/// rejection falls through to the parent `/tables` dir handler, which returns
/// authoritative `NotFound` for names absent from the exhaustive listing.
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

#[omnifs_sdk::path_captures]
struct DatabaseKey {}

#[omnifs_sdk::object(kind = "db.table", key = TableKey, stability = Immutable)]
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
        let backend = match config.database_type {
            DatabaseType::Sqlite => SqliteBackend::open(&config.path, config.read_only)
                .map_err(|e| ProviderError::internal(format!("open sqlite database: {e}")))?,
        };
        install_known_tables(
            backend
                .list_tables()
                .map_err(|e| ProviderError::internal(format!("list tables: {e}")))?,
        );
        let state = State {
            config,
            backend: Rc::new(RefCell::new(backend)),
        };

        r.object::<FileInfo>("/meta", |o| {
            o.representations("info", ())?;
            o.file("version.txt").project(FileInfo::version)?;
            o.file("path.txt").project(FileInfo::path)?;
            Ok(())
        })?;

        r.dir("/tables").handler(tables_list)?;

        r.object::<TableDoc>("/tables/{table}", |o| {
            o.representations("table", ())?;
            o.file("schema.sql").handler(table_schema_sql)?;
            o.file("schema.json").handler(table_schema_json)?;
            o.file("indexes.json").handler(table_indexes_json)?;
            o.file("count.txt").handler(table_count_txt)?;
            o.file("sample.json").handler(table_sample)?;
            Ok(())
        })?;

        Ok(state)
    }
}

impl Key for TableKey {
    type Object = TableDoc;
    type State = State;

    async fn load(
        &self,
        cx: &Cx<Self::State>,
        _since: Option<Validator>,
    ) -> Result<Load<TableDoc>> {
        cx.state(|state| {
            let backend = state.backend.borrow();
            let name = self.table.as_str();
            if !backend
                .table_exists(name)
                .map_err(|e| ProviderError::internal(format!("table_exists: {e}")))?
            {
                return Ok(Load::NotFound);
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
            let validator = backend.table_version(name).ok().map(Validator::from);

            let doc = TableDoc {
                name: name.to_string(),
                create_sql,
                columns: serde_json::to_value(&columns)
                    .map_err(|e| ProviderError::internal(format!("encode columns: {e}")))?,
                indexes: serde_json::to_value(&indexes)
                    .map_err(|e| ProviderError::internal(format!("encode indexes: {e}")))?,
                row_count,
            };
            let mut bytes = serde_json::to_vec_pretty(&doc)
                .map_err(|e| ProviderError::internal(format!("encode table doc: {e}")))?;
            bytes.push(b'\n');
            Ok(Load::Fresh {
                value: doc,
                canonical: Canonical { bytes, validator },
            })
        })
    }
}

impl TableDoc {
    fn schema_sql_bytes(doc: &Self) -> Vec<u8> {
        doc.create_sql.as_deref().unwrap_or("").as_bytes().to_vec()
    }

    fn schema_json_bytes(doc: &Self) -> Result<Vec<u8>> {
        let mut bytes = serde_json::to_vec_pretty(&doc.columns)
            .map_err(|e| ProviderError::internal(format!("encode schema: {e}")))?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    fn indexes_json_bytes(doc: &Self) -> Result<Vec<u8>> {
        let mut bytes = serde_json::to_vec_pretty(&doc.indexes)
            .map_err(|e| ProviderError::internal(format!("encode indexes: {e}")))?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    fn count_bytes(doc: &Self) -> Vec<u8> {
        format!("{}\n", doc.row_count).into_bytes()
    }
}

fn table_leaf_path(table: &str, leaf: &str) -> String {
    format!("/tables/{table}/{leaf}")
}

async fn load_table_doc(cx: &Cx<State>, key: &TableKey) -> Result<TableDoc> {
    match key.load(cx, cx.version().cloned()).await? {
        Load::Fresh { value, .. } => Ok(value),
        Load::Unchanged => Err(ProviderError::internal(
            "table unchanged without a host-pushed canonical in this handler path",
        )),
        Load::NotFound => Err(ProviderError::not_found(format!(
            "table not found: {}",
            key.table.as_str()
        ))),
    }
}

fn table_leaf_projection(
    table: &str,
    leaf: &str,
    bytes: Vec<u8>,
    content_type: Option<ContentType>,
) -> FileProjection {
    let rel = table_leaf_path(table, leaf)
        .trim_start_matches('/')
        .to_string();
    let preload = match content_type {
        Some(ct) => FileProjection::inline(bytes.clone())
            .content_type(ct)
            .build(),
        None => FileProjection::inline(bytes.clone()).build(),
    };
    let mut body = FileProjection::body(bytes);
    if let Some(ct) = content_type {
        body = body.content_type(ct);
    }
    body.preload_file(rel, preload).build()
}

async fn table_schema_sql(cx: Cx<State>, key: TableKey) -> Result<FileProjection> {
    let doc = load_table_doc(&cx, &key).await?;
    let bytes = TableDoc::schema_sql_bytes(&doc);
    Ok(table_leaf_projection(
        key.table.as_str(),
        "schema.sql",
        bytes,
        Some(ContentType::Custom("text/plain")),
    ))
}

async fn table_schema_json(cx: Cx<State>, key: TableKey) -> Result<FileProjection> {
    let doc = load_table_doc(&cx, &key).await?;
    let bytes = TableDoc::schema_json_bytes(&doc)?;
    Ok(table_leaf_projection(
        key.table.as_str(),
        "schema.json",
        bytes,
        Some(ContentType::Json),
    ))
}

async fn table_indexes_json(cx: Cx<State>, key: TableKey) -> Result<FileProjection> {
    let doc = load_table_doc(&cx, &key).await?;
    let bytes = TableDoc::indexes_json_bytes(&doc)?;
    Ok(table_leaf_projection(
        key.table.as_str(),
        "indexes.json",
        bytes,
        Some(ContentType::Json),
    ))
}

async fn table_count_txt(cx: Cx<State>, key: TableKey) -> Result<FileProjection> {
    let doc = load_table_doc(&cx, &key).await?;
    let bytes = TableDoc::count_bytes(&doc);
    Ok(table_leaf_projection(
        key.table.as_str(),
        "count.txt",
        bytes,
        Some(ContentType::Custom("text/plain")),
    ))
}

impl Key for DatabaseKey {
    type Object = FileInfo;
    type State = State;

    async fn load(
        &self,
        cx: &Cx<Self::State>,
        _since: Option<Validator>,
    ) -> Result<Load<FileInfo>> {
        cx.state(|state| {
            let backend = state.backend.borrow();
            let info = backend
                .file_info()
                .map_err(|e| ProviderError::internal(format!("file_info: {e}")))?;
            let validator = backend.meta_version().ok().map(Validator::from);
            let mut bytes = serde_json::to_vec_pretty(&info)
                .map_err(|e| ProviderError::internal(format!("encode info: {e}")))?;
            bytes.push(b'\n');
            Ok(Load::Fresh {
                value: info,
                canonical: Canonical { bytes, validator },
            })
        })
    }
}

impl FileInfo {
    fn version(info: &Self) -> Result<FileContent> {
        Ok(text_content(format!("{}\n", info.sqlite_version)))
    }

    fn path(info: &Self) -> Result<FileContent> {
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

    if bytes.len() > MAX_PROJECTED_BYTES {
        let size = Size::Exact(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        let mut builder = FileProjection::ranged(MemoryRangeReader::new(bytes))
            .size(size)
            .mutable();
        if let Some(v) = version {
            builder = builder.version(v);
        }
        return Ok(builder.build());
    }
    let mut builder = FileProjection::body(bytes).mutable();
    if let Some(v) = version {
        builder = builder.version(v);
    }
    Ok(builder.build())
}

fn text_content(bytes: impl Into<Vec<u8>>) -> FileContent {
    FileContent::new(bytes).with_content_type(ContentType::Custom("text/plain"))
}
