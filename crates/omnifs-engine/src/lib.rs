//! Trusted runtime, cache, and projection engine for omnifs.

pub(crate) mod auth_inject;
pub(crate) mod cache;
pub(crate) mod callouts;
pub(crate) mod capability;
pub(crate) mod clock;
pub mod coalesce;
pub(crate) mod effects;
pub(crate) mod inspect;
pub(crate) mod log_redaction;
pub mod namespace;
pub(crate) mod object_id;
pub(crate) mod ops;
pub(crate) mod pagination;
pub mod render;
pub(crate) mod runtime;
pub(crate) mod sandbox;
pub(crate) mod serving;
pub mod singleflight;
pub mod snapshot;
pub mod test_support;
pub(crate) mod tools;
pub(crate) mod tree;
pub(crate) mod tree_refs;
pub mod view;

pub use callouts::cloner::{CloneError, GitCloner};
pub use inspect::{
    InspectorConfig, InspectorRequestScope, InspectorSink, Subscription, current_trace_id, global,
    init_global_from_env,
};
pub use namespace::{
    Attrs, DirCursor, DirEntry, DirPage, EntryKind as NsEntryKind, Epoch, EventStream, Namespace,
    NodeAnswer, NodeId, NsAttachEvent, NsError, NsEvent, NsRetryClass, ReadAnswer, ReadStyle,
    StabilityClass, TreeNamespace,
};
pub use runtime::registry::{
    FailureKind, MountFailure, MountRuntimes, ReconcileBusy, ReconcileOutcome, RegistryError,
    UpgradeApprovals,
};
pub use runtime::{BuildError, EngineError, HostContext, Runtime as Engine};
pub use serving::ServingContext;
pub use tree::{
    Chunk, Cursor, Entry, EntryOrigin, InvalidationReport, ListOutcome, Listing, Node, NodeBody,
    PaginationControl, RangedHandle, ReadResult, RequestCtx, RetryClass, Synthetic,
    SyntheticContent, Tree, TreeError, TreeErrorKind, spawn_live_follow_pump,
};

pub(crate) use auth_inject as auth;
pub(crate) use cache::blob as blob_cache;
pub(crate) use callouts::wit_convert as wit_protocol;
pub(crate) use callouts::{archive, blob, cloner, git, http};
pub(crate) use effects::apply as effect_apply;
pub(crate) use effects::invalidation;
pub(crate) use inspect as inspector;
pub(crate) use omnifs_wit::provider::Provider;
pub(crate) use ops::op;
pub(crate) use ops::op::Op;
pub(crate) use ops::validate as op_validate;
pub(crate) use runtime::wasm::component_engine;
pub(crate) use runtime::{ProviderErrorClass, Runtime};
pub(crate) use runtime::{instance, registry, wasi, wasm};
