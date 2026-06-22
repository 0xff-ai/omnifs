//! Shared test helpers for wasm fixture construction and path layout.

#[cfg(test)]
use omnifs_core::CredentialId;
#[cfg(test)]
use omnifs_creds::{CredentialEntry, CredentialStore, StoreError};

/// Credential store that rejects listing, for status paths that must not require enumeration.
#[cfg(test)]
pub(crate) struct NonListingStore;

#[cfg(test)]
impl CredentialStore for NonListingStore {
    fn put(&self, _: &CredentialId, _: &CredentialEntry) -> Result<(), StoreError> {
        Ok(())
    }

    fn get(&self, _: &CredentialId) -> Result<Option<CredentialEntry>, StoreError> {
        Ok(None)
    }

    fn delete(&self, _: &CredentialId) -> Result<(), StoreError> {
        Ok(())
    }

    fn list(&self) -> Result<Option<Vec<CredentialId>>, StoreError> {
        Ok(None)
    }

    fn backend_label(&self) -> String {
        "non-listing".to_owned()
    }
}

/// Build a workspace layout rooted at `root` with the standard subdirectory layout
/// our test fixtures use. Directories are not created; callers that need
/// `mounts_dir` or `providers_dir` to exist should mkdir them explicitly.
#[cfg(test)]
pub(crate) fn fixture_paths(root: &std::path::Path) -> omnifs_home::WorkspaceLayout {
    omnifs_home::WorkspaceLayout::under_root(root)
}

#[cfg(test)]
pub(crate) fn wasm_with_provider_metadata(id: &str, provider: &str) -> Vec<u8> {
    let metadata = serde_json::json!({
        "id": id,
        "displayName": id,
        "provider": provider,
        "defaultMount": id,
        "capabilities": [],
        "auth": {
            "inject": {
                "domains": ["api.example.com"],
                "header": "Authorization",
                "prefix": "Bearer "
            },
            "default": "device",
            "schemes": {
                "device": {
                    "type": "oauth",
                    "displayName": "Device flow",
                    "clientId": "client-id",
                    "flow": {
                        "kind": "deviceCode",
                        "authorizationEndpoint": "https://example.com/authorize",
                        "deviceAuthorizationEndpoint": "https://example.com/device/code",
                        "tokenEndpoint": "https://example.com/token"
                    }
                }
            }
        }
    });
    wasm_with_metadata_section(&serde_json::to_vec(&metadata).unwrap())
}

/// Frame raw metadata-section bytes into a minimal wasm module. Use directly to
/// build fixtures with malformed metadata; most tests want
/// [`wasm_with_provider_metadata`].
#[cfg(test)]
pub(crate) fn wasm_with_metadata_section(data: &[u8]) -> Vec<u8> {
    let mut wasm = b"\0asm\x01\0\0\0".to_vec();
    let mut section = Vec::new();
    push_uleb(
        omnifs_provider::PROVIDER_METADATA_SECTION_NAME.len(),
        &mut section,
    );
    section.extend_from_slice(omnifs_provider::PROVIDER_METADATA_SECTION_NAME.as_bytes());
    section.extend_from_slice(data);
    wasm.push(0);
    push_uleb(section.len(), &mut wasm);
    wasm.extend(section);
    wasm
}

#[cfg(test)]
pub(crate) fn push_uleb(mut value: usize, out: &mut Vec<u8>) {
    loop {
        let mut byte = u8::try_from(value & 0x7f).unwrap();
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            break;
        }
    }
}
