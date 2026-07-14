//! Provider manifest declarations and mount-owned scalar resource limits.

#![forbid(unsafe_code)]

mod model;

pub use model::{AccessNeed, LimitDeclarations, Limits, PreopenMode, PreopenedPath, ResourceLimit};
