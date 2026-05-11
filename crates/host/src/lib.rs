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
#[cfg(target_os = "linux")]
pub mod fuse;
#[cfg(target_os = "linux")]
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

impl omnifs::provider::types::ProviderReturn {
    /// True when the provider needs the host to run the staged callouts
    /// and call `resume` with their outcomes.
    pub fn is_suspended(&self) -> bool {
        self.terminal.is_none() && !self.callouts.is_empty()
    }

    /// Unwrap the terminal result, panicking if the response is still
    /// suspended. Intended for test assertions.
    pub fn expect_terminal(self) -> omnifs::provider::types::OpResult {
        match self.terminal {
            Some(op) => op,
            None => panic!("expected terminal, got callouts-only response"),
        }
    }

    /// Take the staged callouts, panicking if the response carries a
    /// terminal. Intended for test assertions.
    pub fn expect_callouts(self) -> Vec<omnifs::provider::types::Callout> {
        assert!(
            self.terminal.is_none(),
            "expected callouts-only response, got terminal"
        );
        self.callouts
    }
}
