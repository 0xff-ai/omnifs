//! declarative router (ADR-0001 §10).

mod dispatch;
mod handlers;
mod object;
mod pattern;
mod projection;
mod register;

#[cfg(test)]
mod tests;

pub use handlers::RouteHandle;
pub use handlers::{
    IntoDirHandler, IntoFileHandler, IntoTreeRefHandler, NoCaptures, WithCaptures, WithKeyMethod,
    WithSyncKeyMethod,
};
pub use object::{DirObjectBlock, FileObjectBlock, ObjectHandle, object};
pub use register::{DirRoute, FileRoute, Router, TreeRefRoute};
