//! Single-import module for providers: `use omnifs_sdk::prelude::*;`

pub use crate::browse::{EntryKind, EventOutcome};
pub use crate::cx::Cx;
pub use crate::cx::join_all;
pub use crate::error::{ProviderError, ProviderErrorKind, Result};
pub use crate::file_attrs::{
    Bytes, FileAttrs, MAX_EAGER_RESPONSE_BYTES, MAX_PROJECTED_BYTES, MAX_VERSION_TOKEN_BYTES,
    ReadMode, Size, Stability, VersionToken,
};
pub use crate::handler::{
    BindCtx, Cursor, DirCx, DirIntent, FileChunk, FileContent, FileStat, Handler,
    MemoryRangeReader, PageStatus, Projection, RangeReader, SubtreeRegistry, TreeRef,
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
