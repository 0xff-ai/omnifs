//! Small return-shape and rendering helpers shared by handlers and the
//! provider macro's generated glue.

use crate::error::{ProviderError, Result};
use omnifs_wit::provider::types::{OpResult, ProviderReturn};

/// Wrap anything convertible to a [`ProviderError`] into a terminal
/// [`ProviderReturn`] with no effects.
pub fn err(error: impl Into<ProviderError>) -> ProviderReturn {
    ProviderReturn::terminal(OpResult::from(error.into()))
}

/// Render a value as pretty-printed JSON with a trailing newline, so the
/// projected file behaves like ordinary text under shell tools (`cat` ends
/// at a line boundary, `wc -l` counts the last line).
pub fn pretty_json<T: serde::Serialize>(value: &T) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value)
        .map_err(|e| ProviderError::internal(format!("JSON encode: {e}")))?;
    bytes.push(b'\n');
    Ok(bytes)
}
