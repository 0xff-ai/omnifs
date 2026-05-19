use std::cell::RefCell;
use std::rc::Rc;

use omnifs_sdk::prelude::*;

use crate::sqlite_backend::SqliteBackend;
use crate::{Config, DatabaseType, State};

#[provider(mounts(crate::meta::MetaHandlers, crate::tables::TableHandlers))]
impl DbProvider {
    fn init(config: Config) -> Result<(State, ProviderInfo)> {
        let backend = match config.database_type {
            DatabaseType::Sqlite => SqliteBackend::open(&config.path, config.read_only)
                .map_err(|e| ProviderError::internal(format!("open sqlite database: {e}")))?,
        };
        let state = State {
            config,
            backend: Rc::new(RefCell::new(backend)),
        };
        Ok((
            state,
            ProviderInfo {
                name: "db-provider".to_string(),
                version: "0.1.0".to_string(),
                description: "Relational database provider (SQLite today)".to_string(),
            },
        ))
    }

    fn capabilities() -> RequestedCapabilities {
        RequestedCapabilities {
            domains: Vec::new(),
            unix_sockets: Vec::new(),
            auth_types: Vec::new(),
            // SQLite's page cache plus rusqlite's prepared statements
            // sit well under 64 MiB for the schemas we expose. The
            // host is the ceiling enforcer; this is the suggestion.
            max_memory_mb: 64,
            needs_git: false,
            needs_websocket: false,
            needs_streaming: false,
            // Nothing in the v1 surface mutates state, so we leave
            // refresh disabled. Future event-driven invalidation
            // (mtime watch on the DB file, etc.) can flip this.
            refresh_interval_secs: 0,
        }
    }
}
