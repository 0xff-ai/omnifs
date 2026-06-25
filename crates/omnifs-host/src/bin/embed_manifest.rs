//! Build-time tool: rewrite each provider component in place so it carries its
//! self-reported manifest as the `omnifs.provider-metadata.v1` custom section
//! the host reads pre-instantiation.
//!
//! It instantiates each component (no `start`, no config) and calls the
//! `manifest_json()` lifecycle export the `#[provider]` macro emits, which
//! returns the full manifest including the `config_schema` the proc-macro could
//! not evaluate. The JSON is validated as a `ProviderManifest` and injected.
//!
//! Usage: `omnifs-embed-manifest <provider.wasm>...`

use std::path::Path;

use omnifs_host::{Instance, component_engine};
use omnifs_provider::{ProviderManifest, embed_provider_metadata_section};

type DynError = Box<dyn std::error::Error>;

fn main() -> Result<(), DynError> {
    let paths: Vec<String> = std::env::args().skip(1).collect();
    if paths.is_empty() {
        return Err("usage: omnifs-embed-manifest <provider.wasm>...".into());
    }
    let engine =
        component_engine(None, |_| {}).map_err(|error| format!("build engine: {error}"))?;
    for path in &paths {
        let path = Path::new(path);
        let wasm =
            std::fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
        let instance = Instance::new(&engine, path, b"{}".to_vec(), &[])?;
        let json = instance.manifest_json()?;
        // Fail the build loudly if a provider self-reports a malformed manifest.
        ProviderManifest::from_bytes(json.as_bytes())
            .map_err(|error| format!("{}: invalid manifest: {error}", path.display()))?;
        let rewritten = embed_provider_metadata_section(&wasm, json.as_bytes())?;
        std::fs::write(path, &rewritten)
            .map_err(|error| format!("write {}: {error}", path.display()))?;
        println!(
            "embedded manifest ({} bytes) into {}",
            json.len(),
            path.display()
        );
    }
    Ok(())
}
