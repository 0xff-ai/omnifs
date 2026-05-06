//! Single-import module for providers: `use omnifs_sdk::prelude::*;`

pub use crate::browse::{EntryKind, EventOutcome, Size};
pub use crate::cx::Cx;
pub use crate::cx::join_all;
pub use crate::error::{ProviderError, ProviderErrorKind, Result};
pub use crate::handler::{
    Cursor, DirCx, DirIntent, FileContent, PageStatus, Projection, SubtreeRef,
};
pub use crate::helpers::err;
pub use crate::init::Init;

// Proc macros (invoked as #[omnifs_sdk::provider] and #[dir]/#[file]/#[subtree])
pub use omnifs_sdk_macros::{config, dir, file, handlers, mutate, provider, subtree};

// Curated WIT types that provider authors and generated code actually use.
pub use crate::omnifs::provider::types::{
    CalloutResults, FileChange, OpResult, PlannedMutation, ProviderEvent, ProviderInfo,
    ProviderReturn, RequestedCapabilities,
};
