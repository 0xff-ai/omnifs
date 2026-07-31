//! In-process KCL authoring for the omnifs resource model.
//!
//! KCL is deliberately kept at this boundary. The evaluator produces an
//! in-memory authoring value; callers convert it to the strict omnifs API
//! types before talking to the daemon.

mod evaluator;
mod source;

pub use evaluator::{EvaluateError, EvaluatedConfig, evaluate};
pub use source::{
    AuthoringConfig, AuthoringResource, FilesystemAuthoring, LocalProviderSource,
    ProviderAuthoring, ProviderSource, SourceResolutionError, resolve_local_source,
};
