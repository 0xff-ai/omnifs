//! Shared test helpers for wasm fixture construction and path layout.

/// Build a workspace layout rooted at `root` with the standard subdirectory layout
/// our test fixtures use. Directories are not created; callers that need
/// `mounts_dir` or `providers_dir` to exist should mkdir them explicitly.
#[cfg(test)]
pub(crate) fn fixture_paths(root: &std::path::Path) -> omnifs_workspace::layout::WorkspaceLayout {
    omnifs_workspace::layout::WorkspaceLayout::under_root(root)
}

/// A `ProviderRef` JSON value pinned to a placeholder id, for building mount
/// spec fixtures whose serving path is never resolved.
#[cfg(test)]
fn provider_ref_value(name: &str) -> serde_json::Value {
    use omnifs_workspace::ids::ProviderId;
    serde_json::json!({
        "id": ProviderId::from_wasm_bytes(name.as_bytes()).to_string(),
        "meta": { "name": name }
    })
}

/// Build a mount `Spec` from a JSON `body` (with no `provider` field) plus a
/// placeholder `ProviderRef` named `name`.
#[cfg(test)]
pub(crate) fn spec_with_provider(name: &str, body: &str) -> omnifs_workspace::mounts::Spec {
    let mut value: serde_json::Value = serde_json::from_str(body).expect("parse test spec body");
    value["provider"] = provider_ref_value(name);
    serde_json::from_value(value).expect("build test spec")
}

/// Build a mount `Spec` from a JSON `body` (no `provider` field) plus an
/// explicit `reference`, for tests that first install a fixture provider.
#[cfg(test)]
pub(crate) fn spec_with_reference(
    reference: &omnifs_workspace::ids::ProviderRef,
    body: &str,
) -> omnifs_workspace::mounts::Spec {
    let mut value: serde_json::Value = serde_json::from_str(body).expect("parse test spec body");
    value["provider"] = serde_json::to_value(reference).expect("serialize provider ref");
    serde_json::from_value(value).expect("build test spec")
}

/// Install a fake provider (built by [`wasm_with_provider_metadata`]) into the
/// content-addressed store under `providers_dir`, returning its pinned
/// reference. The catalog resolves the embedded manifest from the retained
/// artifact, so auth/config resolution works exactly as in production.
#[cfg(test)]
pub(crate) fn install_fixture_provider(
    providers_dir: &std::path::Path,
    name: &str,
) -> omnifs_workspace::ids::ProviderRef {
    use omnifs_workspace::provider::{Artifact, ProviderStore};

    let file = format!("omnifs_provider_{name}.wasm");
    let bytes = wasm_with_provider_metadata(name, &file);
    let artifact = Artifact::from_bytes(file, bytes).expect("parse fixture provider");
    let reference = artifact.reference();
    let store = ProviderStore::new(providers_dir);
    store.retain(&artifact).expect("retain fixture provider");
    reference
}

#[cfg(test)]
pub(crate) fn wasm_with_provider_metadata(id: &str, provider: &str) -> Vec<u8> {
    let metadata = serde_json::json!({
        "id": id,
        "displayName": id,
        "provider": provider,
        "defaultMount": id,
        "refreshIntervalSecs": 0,
        "capabilities": [
            { "kind": "domain", "value": "api.example.com", "why": "Serve authenticated Example API calls." }
        ],
        "auth": {
            "default": "device",
            "schemes": [
                {
                    "oauth": {
                        "key": "device",
                        "displayName": "Device flow",
                        "authorizationEndpoint": "https://example.com/authorize",
                        "tokenEndpoint": "https://example.com/token",
                        "defaultClientId": "client-id",
                        "flow": {
                            "deviceCode": {
                                "deviceAuthorizationEndpoint": "https://example.com/device/code"
                            }
                        },
                        "injectDomains": ["api.example.com"],
                        "injectValuePrefix": "Bearer "
                    }
                }
            ]
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
        omnifs_workspace::provider::PROVIDER_METADATA_SECTION_NAME.len(),
        &mut section,
    );
    section
        .extend_from_slice(omnifs_workspace::provider::PROVIDER_METADATA_SECTION_NAME.as_bytes());
    section.extend_from_slice(data);
    wasm.push(0);
    push_uleb(section.len(), &mut wasm);
    wasm.extend(section);
    wasm
}

#[cfg(test)]
fn push_uleb(mut value: usize, out: &mut Vec<u8>) {
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
