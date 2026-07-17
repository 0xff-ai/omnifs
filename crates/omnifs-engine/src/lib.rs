//! Trusted runtime, cache, and projection engine for omnifs.

#[cfg(feature = "runtime")]
pub(crate) mod auth_inject;
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
pub mod render;
#[cfg(feature = "runtime")]
pub(crate) mod runtime;
#[cfg(feature = "runtime")]
pub(crate) mod sandbox;
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
pub use inspect::{Inspector, InspectorLayer, Subscription, init_global_from_env};
#[cfg(feature = "runtime")]
pub use namespace::TreeNamespace;
pub use namespace::{
    Attrs, DirCursor, DirEntry, DirPage, EntryKind, EventStream, LookupAnswer, Namespace, NsError,
    NsEvent, NsRetryClass, ReadAnswer, ReadStyle, StabilityClass,
};
#[cfg(feature = "runtime")]
pub use runtime::registry::{MountTable, RegistryError};
#[cfg(feature = "runtime")]
pub use runtime::wasm::{ComponentEngine, WarmOutcome};
#[cfg(feature = "runtime")]
pub use runtime::{BuildError, EngineError, HostContext, Runtime as Engine};
#[cfg(feature = "runtime")]
pub(crate) use tree::{Cursor, Node, RequestCtx, TreeError, TreeErrorKind, spawn_live_follow_pump};

#[cfg(feature = "runtime")]
pub(crate) use auth_inject as auth;
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
