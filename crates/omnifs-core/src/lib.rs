//! Core omnifs protocol types.

mod content_type;
pub mod path;
pub mod view;

pub use content_type::ContentType;
pub use path::{ParseError, Path, Segment};
