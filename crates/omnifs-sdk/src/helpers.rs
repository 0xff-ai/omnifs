//! Common response helpers for providers.

use crate::error::{ProviderError, Result};
use omnifs_wit::provider::types::{OpResult, ProviderReturn, ProviderStep};

/// Build a terminal provider return carrying the given error.
pub fn err(error: impl Into<ProviderError>) -> ProviderReturn {
    ProviderReturn::terminal(OpResult::from(error.into()))
}

/// Build a returned provider step carrying the given error.
pub fn err_step(error: impl Into<ProviderError>) -> ProviderStep {
    ProviderStep::returned(err(error))
}

/// Pretty-print a JSON-serializable value with a trailing newline.
pub fn pretty_json<T: serde::Serialize>(value: &T) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value)
        .map_err(|e| ProviderError::internal(format!("JSON encode: {e}")))?;
    bytes.push(b'\n');
    Ok(bytes)
}
