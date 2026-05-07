//! Single-import module for providers: `use omnifs_sdk::prelude::*;`

pub use crate::browse::{EntryKind, EventOutcome};
pub use crate::cx::Cx;
pub use crate::cx::join_all;
pub use crate::error::{ProviderError, ProviderErrorKind, Result};
pub use crate::handler::{
    BindCtx, Cursor, DirCx, DirIntent, FileContent, FileStat, Handler, PageStatus, Projection,
    SubtreeRegistry, TreeRef,
};
pub use crate::helpers::err;
pub use crate::init::Init;

// Proc macros (invoked as #[omnifs_sdk::provider] and
// #[dir]/#[file]/#[treeref]/#[subtree]/#[bind])
pub use omnifs_sdk_macros::{
    bind, config, dir, file, handlers, mutate, provider, subtree, treeref,
};

// Curated WIT types that provider authors and generated code actually use.
pub use crate::omnifs::provider::types::{
    CalloutResults, FileChange, OpResult, PlannedMutation, ProviderEvent, ProviderInfo,
    ProviderReturn, RequestedCapabilities,
};
