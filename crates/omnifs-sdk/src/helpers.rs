//! Common response helpers for providers.

use crate::browse::Effects;
use crate::error::ProviderError;
use crate::omnifs::provider::types::{Callout, OpResult, ProviderReturn, ProviderStep};

impl ProviderReturn {
    /// Terminal return with no host-side effects.
    pub fn terminal(result: OpResult) -> Self {
        Self {
            result,
            effects: Vec::new(),
        }
    }

    /// Terminal return with effects committed if the return is accepted.
    pub fn with_effects(result: OpResult, effects: Effects) -> Self {
        Self {
            result,
            effects: effects.into_wit(),
        }
    }
}

impl ProviderStep {
    /// Suspension: callouts to run before the host calls `resume`.
    pub fn suspend(callouts: Vec<Callout>) -> Self {
        Self::Suspended(callouts)
    }

    /// Completed operation answer.
    pub fn returned(ret: ProviderReturn) -> Self {
        Self::Returned(ret)
    }
}

/// Build a terminal provider return carrying the given error.
pub fn err(error: impl Into<ProviderError>) -> ProviderReturn {
    ProviderReturn::terminal(OpResult::from(error.into()))
}

/// Build a returned provider step carrying the given error.
pub fn err_step(error: impl Into<ProviderError>) -> ProviderStep {
    ProviderStep::returned(err(error))
}
