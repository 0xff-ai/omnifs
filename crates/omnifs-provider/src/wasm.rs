//! Loaded provider WASM bytes with shared manifest/metadata decode helpers.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use omnifs_core::{IdError, ProviderId, ProviderMeta, ProviderName, ProviderRef, ProviderVersion};

use crate::manifest::ProviderManifest;
use crate::records::{DecodeError, ManifestRecord, ManifestRecordIter};
use crate::sections::{
    ManifestSectionError, ProviderMetadataError, read_manifest_section,
    read_provider_metadata_section,
};

pub struct ProviderWasm {
    bytes: Vec<u8>,
}

/// A provider WASM artifact whose embedded metadata has been validated.
#[derive(Debug)]
pub struct Artifact {
    pub(crate) file: String,
    pub(crate) bytes: Vec<u8>,
    pub(crate) id: ProviderId,
    pub(crate) meta: ProviderMeta,
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderWasmError {
    #[error(transparent)]
    Section(#[from] ManifestSectionError),
    #[error(transparent)]
    Decode(#[from] DecodeError),
}

#[derive(Debug, thiserror::Error)]
pub enum ArtifactError {
    #[error(transparent)]
    Metadata(#[from] ProviderMetadataError),
    #[error("has no embedded provider metadata section")]
    MissingMetadata,
    #[error("manifest id `{id}` is not a valid provider name: {source}")]
    InvalidProviderName { id: String, source: IdError },
}

#[derive(Debug, thiserror::Error)]
pub enum ArtifactLoadError {
    #[error("provider artifact {} has no file name", path.display())]
    MissingFileName { path: PathBuf },
    #[error("read provider artifact {}: {source}", path.display())]
    Io { path: PathBuf, source: io::Error },
    #[error(transparent)]
    Artifact(#[from] ArtifactError),
}

impl ProviderWasm {
    #[must_use]
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }

    #[must_use]
    fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    #[must_use]
    fn into_bytes(self) -> Vec<u8> {
        self.bytes
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

impl Artifact {
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, ArtifactLoadError> {
        let path = path.as_ref();
        let file = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned)
            .ok_or_else(|| ArtifactLoadError::MissingFileName {
                path: path.to_path_buf(),
            })?;
        let bytes = fs::read(path).map_err(|source| ArtifactLoadError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(Self::from_bytes(file, bytes)?)
    }

    pub fn from_bytes(file: impl Into<String>, bytes: Vec<u8>) -> Result<Self, ArtifactError> {
        let file = file.into();
        let wasm = ProviderWasm::from_bytes(bytes);
        let id = ProviderId::from_wasm_bytes(wasm.bytes());
        let manifest = wasm.metadata()?.ok_or(ArtifactError::MissingMetadata)?;
        let name = ProviderName::new(manifest.id.clone()).map_err(|source| {
            ArtifactError::InvalidProviderName {
                id: manifest.id.clone(),
                source,
            }
        })?;
        let meta = ProviderMeta {
            name,
            version: manifest.version.map(ProviderVersion::new),
        };
        Ok(Self {
            file,
            bytes: wasm.into_bytes(),
            id,
            meta,
        })
    }

    #[must_use]
    pub fn file(&self) -> &str {
        &self.file
    }

    #[must_use]
    pub fn id(&self) -> ProviderId {
        self.id
    }

    #[must_use]
    pub fn meta(&self) -> &ProviderMeta {
        &self.meta
    }

    #[must_use]
    pub fn reference(&self) -> ProviderRef {
        ProviderRef {
            id: self.id,
            meta: self.meta.clone(),
        }
    }
}

fn collect_manifest_records(section: &[u8]) -> Result<Vec<ManifestRecord>, DecodeError> {
    let mut records = Vec::new();
    for record in ManifestRecordIter::new(section) {
        records.push(record?);
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sections::embed_provider_metadata_section;

    const EMPTY_WASM: &[u8] = b"\0asm\x01\0\0\0";
    const DEMO_METADATA: &[u8] = br#"{
        "id": "demo",
        "displayName": "Demo",
        "provider": "demo.wasm",
        "defaultMount": "demo"
    }"#;

    #[test]
    fn artifact_from_bytes_reads_metadata_and_hashes_bytes() {
        let bytes = embed_provider_metadata_section(EMPTY_WASM, DEMO_METADATA).unwrap();
        let artifact = Artifact::from_bytes("demo.wasm", bytes.clone()).unwrap();

        assert_eq!(artifact.file(), "demo.wasm");
        assert_eq!(artifact.id(), ProviderId::from_wasm_bytes(&bytes));
        assert_eq!(artifact.meta().name.as_str(), "demo");
        assert!(artifact.meta().version.is_none());
    }

    #[test]
    fn artifact_from_bytes_rejects_missing_metadata() {
        let error = Artifact::from_bytes("stale.wasm", EMPTY_WASM.to_vec()).unwrap_err();

        assert!(matches!(error, ArtifactError::MissingMetadata));
    }
}
