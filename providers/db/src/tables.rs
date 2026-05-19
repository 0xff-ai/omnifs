//! `/tables/` index + per-table bind site.

use omnifs_sdk::prelude::*;

use crate::table_subtree::{TableName, TableSubtree};
use crate::{Result, State};

pub struct TableHandlers;

#[handlers]
impl TableHandlers {
    #[dir("/tables")]
    fn list(cx: &DirCx<State>) -> Result<Projection> {
        let names = cx.state(|state| {
            state
                .backend
                .borrow()
                .list_tables()
                .map_err(|e| ProviderError::internal(format!("list tables: {e}")))
        })?;
        let mut p = Projection::new();
        for name in names {
            p.dir(name);
        }
        p.page(PageStatus::Exhaustive);
        Ok(p)
    }

    #[bind("/tables/{name}")]
    fn table(_cx: &Cx<State>, name: TableName) -> Result<TableSubtree> {
        Ok(TableSubtree {
            name: name.into_inner(),
        })
    }
}
