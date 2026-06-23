use std::path::Path;

use omnifs_provider as mts;

/// Decoded provider component artifact with embedded metadata accessors.
pub struct Artifact {
    wasm: mts::ProviderWasm,
}

impl Artifact {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref();
        let bytes =
            std::fs::read(path).map_err(|error| format!("reading {}: {error}", path.display()))?;
        Ok(Self {
            wasm: mts::ProviderWasm::from_bytes(bytes),
        })
    }

    pub fn metadata(&self) -> Result<Option<mts::ProviderManifest>, String> {
        self.wasm.metadata().map_err(|error| error.to_string())
    }
}
