//! Trusted runtime, cache, and projection engine for omnifs.

#[cfg(feature = "runtime")]
pub(crate) mod authority;
#[cfg(feature = "runtime")]
pub(crate) mod cache;
#[cfg(feature = "runtime")]
pub(crate) mod callouts;
#[cfg(feature = "runtime")]
pub(crate) mod clock;
#[cfg(feature = "runtime")]
pub(crate) mod effects;
#[cfg(feature = "runtime")]
mod generation;
#[cfg(feature = "runtime")]
pub mod inspect;
#[cfg(feature = "runtime")]
pub(crate) mod log_redaction;
pub mod namespace;
#[cfg(feature = "runtime")]
pub(crate) mod object_id;
#[cfg(feature = "runtime")]
pub(crate) mod ops;
#[cfg(feature = "runtime")]
pub(crate) mod pagination;
#[cfg(feature = "runtime")]
pub(crate) mod runtime;
#[cfg(feature = "runtime")]
pub mod test_support;
#[cfg(feature = "runtime")]
pub(crate) mod tools;
#[cfg(feature = "runtime")]
pub(crate) mod tree;
#[cfg(feature = "runtime")]
pub(crate) mod tree_refs;
pub mod view;

#[cfg(feature = "runtime")]
pub use callouts::cloner::{CloneError, GitCloner};
#[cfg(feature = "runtime")]
pub use generation::{
    CredentialProvenance, DrainOutcome, GenerationProvenance, MountProvenance, PreparedGeneration,
    PublishReadyGeneration, RetiredGeneration, ServingCell, ServingMountStatus,
};
#[cfg(feature = "runtime")]
pub use inspect::{Inspector, InspectorLayer, Subscription, init_global_from_env};
#[cfg(feature = "runtime")]
pub use namespace::EngineNamespace;
pub use namespace::{
    Attrs, DirCursor, DirEntry, DirPage, EntryKind, EventStream, LookupAnswer, LookupState,
    Namespace, NsError, NsEvent, NsRetryClass, ReadAnswer, ReadStyle, Stability,
};
#[cfg(feature = "runtime")]
pub use runtime::registry::{
    MountAvailability, MountBuildInput, MountBuildState, MountTable, ProviderBuildInput,
    RegistryError,
};
#[cfg(feature = "runtime")]
pub use runtime::wasm::ComponentEngine;
#[cfg(feature = "runtime")]
pub use runtime::{
    BuildError, EngineError, HostError, HostOnline, HostRuntimeOpen, Runtime as Engine,
    RuntimeMountConfig,
};
#[cfg(feature = "runtime")]
pub(crate) use tree::{Cursor, Node, RequestCtx, TreeError, TreeErrorKind, spawn_live_follow_pump};

#[cfg(feature = "runtime")]
pub(crate) use callouts::wit_convert as wit_protocol;
#[cfg(feature = "runtime")]
pub(crate) use callouts::{blob, cloner, git, http};
#[cfg(feature = "runtime")]
pub(crate) use effects::apply as effect_apply;
#[cfg(feature = "runtime")]
pub(crate) use effects::invalidation;
#[cfg(feature = "runtime")]
pub(crate) use inspect as inspector;
#[cfg(feature = "runtime")]
pub(crate) use omnifs_wit::provider::Provider;
#[cfg(feature = "runtime")]
pub(crate) use ops::validate as op_validate;
#[cfg(feature = "runtime")]
#[cfg(feature = "runtime")]
pub(crate) use runtime::{ProviderErrorClass, Runtime};
#[cfg(feature = "runtime")]
pub(crate) use runtime::{instance, registry, wasi};
