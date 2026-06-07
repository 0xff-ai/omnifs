//! Common response helpers for providers.

use crate::error::ProviderError;
use omnifs_wit::provider::types::{OpResult, ProviderReturn, ProviderStep};

/// Build a terminal provider return carrying the given error.
pub fn err(error: impl Into<ProviderError>) -> ProviderReturn {
    ProviderReturn::terminal(OpResult::from(error.into()))
}

/// Build a returned provider step carrying the given error.
pub fn err_step(error: impl Into<ProviderError>) -> ProviderStep {
    ProviderStep::returned(err(error))
}
