//! Build-time metadata harvester.
//!
//! Links every provider crate as a native library, calls its
//! `provider_metadata()` accessor (which the `#[provider]` macro emits for
//! non-wasm targets and which returns the host's [`ProviderManifest`]),
//! serializes it with `serde_json`, and injects the bytes as the
//! `omnifs.provider-metadata.v1` custom section into the already-built wasm
//! component. The provider's metadata is stamped verbatim: there is no
//! translation step, because what the provider constructs is the wire type the
//! host reads back. The host reads that section pre-instantiation; this tool
//! never instantiates a component.
//!
//! Usage: `omnifs-embed-metadata <wasm-dir>`.

use std::collections::HashSet;
use std::path::Path;

use omnifs_workspace::provider::{
    ProviderManifest, embed_provider_metadata_section, read_provider_metadata_section,
};

type DynError = Box<dyn std::error::Error>;

/// A provider's wasm filename paired with its metadata accessor.
type ProviderEntry = (&'static str, fn() -> ProviderManifest);

/// The providers to embed metadata into. Adding a provider is one line here.
const PROVIDERS: &[ProviderEntry] = &[
    (
        "omnifs_provider_arxiv.wasm",
        omnifs_provider_arxiv::provider_metadata,
    ),
    (
        "omnifs_provider_db.wasm",
        omnifs_provider_db::provider_metadata,
    ),
    (
        "omnifs_provider_dns.wasm",
        omnifs_provider_dns::provider_metadata,
    ),
    (
        "omnifs_provider_docker.wasm",
        omnifs_provider_docker::provider_metadata,
    ),
    (
        "omnifs_provider_github.wasm",
        omnifs_provider_github::provider_metadata,
    ),
    (
        "omnifs_provider_kubernetes.wasm",
        omnifs_provider_kubernetes::provider_metadata,
    ),
    (
        "omnifs_provider_linear.wasm",
        omnifs_provider_linear::provider_metadata,
    ),
    (
        "omnifs_provider_oura.wasm",
        omnifs_provider_oura::provider_metadata,
    ),
    (
        "omnifs_provider_web.wasm",
        omnifs_provider_web::provider_metadata,
    ),
    ("test_provider.wasm", test_provider::provider_metadata),
];

fn main() -> Result<(), DynError> {
    let dir = std::env::args()
        .nth(1)
        .ok_or("usage: omnifs-embed-metadata <wasm-dir>")?;
    let dir = Path::new(&dir);

    for (file, metadata) in PROVIDERS {
        let path = dir.join(file);
        let wasm =
            std::fs::read(&path).map_err(|error| format!("read {}: {error}", path.display()))?;
        let json = serde_json::to_vec(&metadata())
            .map_err(|error| format!("{}: serialize manifest: {error}", path.display()))?;
        let rewritten = embed_provider_metadata_section(&wasm, &json)?;
        // Validate the embedded artifact exactly as the host will read it: this
        // gates on schema + domain validation AND catches a stray duplicate
        // section (e.g. a stale nested one) before a bad wasm is written.
        read_provider_metadata_section(&rewritten)
            .map_err(|error| format!("{}: invalid embedded metadata: {error}", path.display()))?
            .ok_or_else(|| format!("{}: no metadata section after embed", path.display()))?;
        std::fs::write(&path, &rewritten)
            .map_err(|error| format!("write {}: {error}", path.display()))?;
        println!(
            "embedded metadata ({} bytes) into {}",
            json.len(),
            path.display()
        );
    }

    // Guard against the PROVIDERS registry drifting from the built wasm set: any
    // provider component in the dir we did not embed would ship metadata-less and
    // only fail at host load, with no build-time signal. Fail here instead.
    let embedded: HashSet<&str> = PROVIDERS.iter().map(|(file, _)| *file).collect();
    for entry in
        std::fs::read_dir(dir).map_err(|error| format!("scan {}: {error}", dir.display()))?
    {
        let path = entry
            .map_err(|error| format!("scan {}: {error}", dir.display()))?
            .path();
        if path.extension().is_none_or(|ext| ext != "wasm") {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let is_provider_component =
            name.starts_with("omnifs_provider_") || name == "test_provider.wasm";
        if is_provider_component && !embedded.contains(name) {
            return Err(format!(
                "{name} is a provider component but is not in the embed registry; \
                 add it to PROVIDERS in omnifs-embed-metadata"
            )
            .into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::PROVIDERS;

    /// Every provider's metadata must serialize and survive the host's
    /// validating deserializer unchanged: the harvester stamps it verbatim, so
    /// the wire shape a provider constructs is exactly what the host reads back.
    #[test]
    fn every_provider_metadata_round_trips_through_host_validation() {
        for (file, metadata) in PROVIDERS {
            let manifest = metadata();
            let json = serde_json::to_vec(&manifest).expect("serialize manifest");
            let parsed = omnifs_workspace::provider::ProviderManifest::from_bytes(&json)
                .unwrap_or_else(|err| {
                    panic!(
                        "{file}: host rejected metadata: {err}\njson: {}",
                        String::from_utf8_lossy(&json)
                    )
                });
            assert_eq!(parsed.id, manifest.id, "{file}: id round-trip");
            assert_eq!(
                parsed.auth.is_some(),
                manifest.auth.is_some(),
                "{file}: auth round-trip"
            );
            assert_eq!(
                parsed.config.is_some(),
                manifest.config.is_some(),
                "{file}: config round-trip"
            );
        }
    }
}
