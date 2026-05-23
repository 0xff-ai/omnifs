use std::cell::RefCell;
use std::rc::Rc;

use omnifs_sdk::prelude::*;

use crate::sqlite_backend::SqliteBackend;
use crate::{Config, DatabaseType, State};

#[provider(
    metadata = "omnifs.provider.json",
    mounts(crate::meta::MetaHandlers, crate::tables::TableHandlers)
)]
impl DbProvider {
    fn init(config: Config) -> Result<(State, ProviderInfo, RequestedCapabilities)> {
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
            RequestedCapabilities::empty(),
        ))
    }
}
