//! Loaded provider WASM bytes with shared manifest/metadata decode helpers.

use crate::manifest::ProviderManifest;
use crate::records::{DecodeError, ManifestRecord, ManifestRecordIter};
use crate::sections::{
    ManifestSectionError, ProviderMetadataError, read_manifest_section,
    read_provider_metadata_section,
};

pub struct ProviderWasm {
    bytes: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderWasmError {
    #[error(transparent)]
    Section(#[from] ManifestSectionError),
    #[error(transparent)]
    Decode(#[from] DecodeError),
}

impl ProviderWasm {
    #[must_use]
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }

    pub fn manifest_section(&self) -> Result<Vec<u8>, ManifestSectionError> {
        read_manifest_section(&self.bytes)
    }

    pub fn metadata(&self) -> Result<Option<ProviderManifest>, ProviderMetadataError> {
        read_provider_metadata_section(&self.bytes)
    }

    pub fn manifest_records(&self) -> Result<Vec<ManifestRecord>, ProviderWasmError> {
        let section_bytes = self.manifest_section()?;
        Ok(collect_manifest_records(&section_bytes)?)
    }
}

fn collect_manifest_records(section: &[u8]) -> Result<Vec<ManifestRecord>, DecodeError> {
    let mut records = Vec::new();
    for record in ManifestRecordIter::new(section) {
        records.push(record?);
    }
    Ok(records)
}
