//! omnifs-host: Host runtime for the omnifs virtual filesystem.
//!
//! This crate provides the infrastructure for running WASM-based filesystem
//! providers via the WebAssembly Component Model. Key components include:
//!
//! - `registry`: Provider loading and lifecycle management
//! - `runtime`: Callout execution (HTTP, Git, KV operations)
//! - `fuse`: Linux FUSE filesystem implementation
//! - `auth`: Authentication and credential injection
//! - `config`: Instance configuration and schema validation

pub mod auth;
pub mod cache;
pub mod config;
pub mod fuse;
pub mod mount;
pub mod path_key;
pub(crate) mod path_prefix;
pub mod registry;
pub mod runtime;

wasmtime::component::bindgen!({
    path: "../../wit",
    world: "provider",
    additional_derives: [Clone],
});

pub(crate) mod extractor_bindings {
    wasmtime::component::bindgen!({
        path: "../../wit/extractor",
        world: "extractor",
    });
}

impl omnifs::provider::types::ProviderStep {
    /// True when the provider needs the host to run callouts and resume
    /// the same operation.
    pub fn is_suspended(&self) -> bool {
        matches!(self, Self::Suspended(callouts) if !callouts.is_empty())
    }

    /// Unwrap the terminal result, panicking if the step is suspended.
    /// Intended for test assertions.
    pub fn expect_returned(self) -> omnifs::provider::types::ProviderReturn {
        match self {
            Self::Returned(ret) => ret,
            Self::Suspended(_) => panic!("expected returned provider step, got suspended"),
        }
    }

    /// Take the staged callouts, panicking if the step is terminal.
    /// Intended for test assertions.
    pub fn expect_callouts(self) -> Vec<omnifs::provider::types::Callout> {
        match self {
            Self::Suspended(callouts) => callouts,
            Self::Returned(_) => panic!("expected suspended provider step, got returned"),
        }
    }
}

impl omnifs::provider::types::ProviderReturn {
    /// Unwrap the operation result. Intended for test assertions.
    pub fn expect_result(self) -> omnifs::provider::types::OpResult {
        self.result
    }
}
