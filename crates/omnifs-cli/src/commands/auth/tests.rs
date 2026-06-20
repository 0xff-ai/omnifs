use super::import::import_static_token_value;
use super::login::manual_code_from_input;
use super::status::status;
use crate::catalog::ProviderCatalog;
use crate::session::MountConfig;
use crate::workspace::Workspace;
use omnifs_core::CredentialId;
use omnifs_creds::{CredentialStore, MemoryStore};
use omnifs_home::WorkspaceLayout;
use omnifs_provider::{AuthManifest, AuthScheme, OAuthFlow, OauthScheme, StaticTokenScheme};
use secrecy::{ExposeSecret, SecretString};
use std::path::Path;

fn mounts_for(paths: &WorkspaceLayout) -> Vec<MountConfig> {
    Workspace::from_layout(paths.clone()).mounts().unwrap()
}

#[test]
fn static_token_import_stores_typed_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = fixture_paths(tmp.path());
    write_mount(
        &paths,
        "github",
        r#"{
                "provider":"github.wasm",
                "mount":"github",
                "auth":{"type":"static-token","scheme":"pat"}
            }"#,
    );
    write_provider(&paths, None);
    let store = MemoryStore::new();
    let catalog = ProviderCatalog::for_providers(&paths.providers_dir);
    let mounts = mounts_for(&paths);

    import_static_token_value(
        &catalog,
        &mounts,
        &store,
        "github",
        SecretString::from("secret".to_owned()),
        Some("pat"),
        Some("me"),
    )
    .unwrap();

    let key = CredentialId::new("github", "pat", "me").unwrap();
    assert_eq!(
        store
            .get(&key)
            .unwrap()
            .unwrap()
            .access_token()
            .expose_secret(),
        "secret"
    );
}

#[test]
fn status_does_not_require_store_listing() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = fixture_paths(tmp.path());
    write_mount(
        &paths,
        "github",
        r#"{"provider":"github.wasm","mount":"github","auth":{"type":"static-token","scheme":"pat"}}"#,
    );
    write_provider(&paths, None);
    let catalog = ProviderCatalog::for_providers(&paths.providers_dir);
    let mounts = mounts_for(&paths);
    status(
        &paths,
        &catalog,
        mounts,
        &crate::test_support::NonListingStore,
    )
    .unwrap();
}

#[test]
fn schemes_reads_manifest_from_provider() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = fixture_paths(tmp.path());
    write_mount(
        &paths,
        "github",
        r#"{"provider":"github.wasm","mount":"github","auth":{"type":"static-token"}}"#,
    );
    let manifest = AuthManifest {
        schemes: vec![AuthScheme::StaticToken(StaticTokenScheme {
            key: "pat".to_owned(),
            header_name: Some("Authorization".to_owned()),
            value_prefix: "Bearer ".to_owned(),
            description: "token".to_owned(),
            inject_domains: vec!["api.github.com".to_owned()],
            creation_url: None,
            validation: None,
        })],
    };
    write_provider(&paths, Some(&manifest));

    let catalog = ProviderCatalog::for_providers(&paths.providers_dir);
    let mounts = mounts_for(&paths);
    let mount_auth = catalog.load_mount_auth(&mounts, "github").unwrap();
    let loaded = catalog
        .auth_manifest_for(mount_auth.config())
        .unwrap()
        .unwrap();

    assert_eq!(loaded, manifest);
}

#[test]
fn oauth_request_uses_builtin_github_manifest_without_provider_file() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = fixture_paths(tmp.path());
    write_mount(
        &paths,
        "github",
        r#"{
                "provider":"omnifs_provider_github.wasm",
                "mount":"github",
                "auth":{
                    "type":"oauth",
                    "scheme":"device"
                }
            }"#,
    );

    let catalog = ProviderCatalog::for_providers(&paths.providers_dir);
    let mounts = mounts_for(&paths);
    let mount = catalog.load_mount_auth(&mounts, "github").unwrap();
    let (request, target) = mount.oauth_request(None, &[]).unwrap();

    assert_eq!(target.primary_key().unwrap().provider_id(), "github");
    assert_eq!(request.scheme().key, "device");
    assert_eq!(
        request.scheme().default_client_id.as_deref(),
        Some("Ov23licogxMDzS47s9sF")
    );
    assert!(request.scheme().default_scopes.is_empty());
    assert!(matches!(request.scheme().flow, OAuthFlow::DeviceCode(_)));
}

#[test]
fn oauth_request_uses_configured_client_id_when_manifest_has_no_default() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = fixture_paths(tmp.path());
    write_mount(
        &paths,
        "example",
        r#"{
                "provider":"github.wasm",
                "mount":"example",
                "auth":{
                    "type":"oauth",
                    "scheme":"oauth",
                    "clientId":"byo-client",
                    "scopes":["read","write"]
                }
            }"#,
    );
    let manifest = AuthManifest {
        schemes: vec![AuthScheme::Oauth(OauthScheme {
            key: "oauth".to_owned(),
            display_name: "OAuth".to_owned(),
            authorization_endpoint: "https://example.com/authorize".to_owned(),
            token_endpoint: "https://example.com/token".to_owned(),
            revocation_endpoint: None,
            default_client_id: None,
            default_scopes: vec!["read".to_owned()],
            flow: OAuthFlow::PkceManualCode(omnifs_provider::PkceManualCodeConfig {
                redirect_uri: "http://localhost/callback".to_owned(),
            }),
            token_endpoint_auth: omnifs_provider::TokenEndpointAuthMethod::None,
            refresh_token_rotates: true,
            extra_authorize_params: vec![],
            extra_token_params: vec![],
            inject_domains: vec!["example.com".to_owned()],
            inject_header_name: None,
            inject_value_prefix: "Bearer ".to_owned(),
        })],
    };
    write_provider(&paths, Some(&manifest));

    let catalog = ProviderCatalog::for_providers(&paths.providers_dir);
    let mounts = mounts_for(&paths);
    let mount = catalog.load_mount_auth(&mounts, "example").unwrap();
    let (request, _target) = mount.oauth_request(None, &[]).unwrap();

    assert_eq!(request.client_id(), Some("byo-client"));
    assert_eq!(
        request.scheme().default_scopes,
        vec!["read".to_owned(), "write".to_owned()]
    );
}

#[test]
fn oauth_request_uses_provider_default_client_id_when_config_omits_it() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = fixture_paths(tmp.path());
    write_mount(
        &paths,
        "example",
        r#"{
                "provider":"github.wasm",
                "mount":"example",
                "auth":{
                    "type":"oauth",
                    "scheme":"oauth"
                }
            }"#,
    );
    let manifest = AuthManifest {
        schemes: vec![AuthScheme::Oauth(OauthScheme {
            key: "oauth".to_owned(),
            display_name: "OAuth".to_owned(),
            authorization_endpoint: "https://example.com/authorize".to_owned(),
            token_endpoint: "https://example.com/token".to_owned(),
            revocation_endpoint: None,
            default_client_id: Some("provider-client".to_owned()),
            default_scopes: vec!["read".to_owned()],
            flow: OAuthFlow::PkceManualCode(omnifs_provider::PkceManualCodeConfig {
                redirect_uri: "http://localhost/callback".to_owned(),
            }),
            token_endpoint_auth: omnifs_provider::TokenEndpointAuthMethod::None,
            refresh_token_rotates: true,
            extra_authorize_params: vec![],
            extra_token_params: vec![],
            inject_domains: vec!["example.com".to_owned()],
            inject_header_name: None,
            inject_value_prefix: "Bearer ".to_owned(),
        })],
    };
    write_provider(&paths, Some(&manifest));

    let catalog = ProviderCatalog::for_providers(&paths.providers_dir);
    let mounts = mounts_for(&paths);
    let mount = catalog.load_mount_auth(&mounts, "example").unwrap();
    let (request, _target) = mount.oauth_request(None, &[]).unwrap();

    assert_eq!(request.client_id(), None);
    assert_eq!(
        request.scheme().default_client_id.as_deref(),
        Some("provider-client")
    );
    assert_eq!(request.scheme().default_scopes, vec!["read".to_owned()]);
}

#[test]
fn oauth_request_cli_scopes_override_config_scopes() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = fixture_paths(tmp.path());
    write_mount(
        &paths,
        "example",
        r#"{
                "provider":"github.wasm",
                "mount":"example",
                "auth":{
                    "type":"oauth",
                    "scheme":"oauth",
                    "scopes":["read"]
                }
            }"#,
    );
    let manifest = AuthManifest {
        schemes: vec![AuthScheme::Oauth(OauthScheme {
            key: "oauth".to_owned(),
            display_name: "OAuth".to_owned(),
            authorization_endpoint: "https://example.com/authorize".to_owned(),
            token_endpoint: "https://example.com/token".to_owned(),
            revocation_endpoint: None,
            default_client_id: Some("provider-client".to_owned()),
            default_scopes: vec![],
            flow: OAuthFlow::PkceManualCode(omnifs_provider::PkceManualCodeConfig {
                redirect_uri: "http://localhost/callback".to_owned(),
            }),
            token_endpoint_auth: omnifs_provider::TokenEndpointAuthMethod::None,
            refresh_token_rotates: true,
            extra_authorize_params: vec![],
            extra_token_params: vec![],
            inject_domains: vec!["example.com".to_owned()],
            inject_header_name: None,
            inject_value_prefix: "Bearer ".to_owned(),
        })],
    };
    write_provider(&paths, Some(&manifest));

    let catalog = ProviderCatalog::for_providers(&paths.providers_dir);
    let mounts = mounts_for(&paths);
    let mount = catalog.load_mount_auth(&mounts, "example").unwrap();
    let (request, _target) = mount.oauth_request(None, &["repo".to_owned()]).unwrap();

    assert_eq!(request.scheme().default_scopes, vec!["repo".to_owned()]);
}

#[test]
fn oauth_request_uses_provider_metadata_id_for_credential_id() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = fixture_paths(tmp.path());
    write_mount(
        &paths,
        "example",
        r#"{
                "provider":"github.wasm",
                "mount":"example",
                "auth":{
                    "type":"oauth",
                    "scheme":"oauth"
                }
            }"#,
    );
    let manifest = AuthManifest {
        schemes: vec![AuthScheme::Oauth(OauthScheme {
            key: "oauth".to_owned(),
            display_name: "OAuth".to_owned(),
            authorization_endpoint: "https://example.com/authorize".to_owned(),
            token_endpoint: "https://example.com/token".to_owned(),
            revocation_endpoint: None,
            default_client_id: Some("provider-client".to_owned()),
            default_scopes: vec![],
            flow: OAuthFlow::PkceManualCode(omnifs_provider::PkceManualCodeConfig {
                redirect_uri: "http://localhost/callback".to_owned(),
            }),
            token_endpoint_auth: omnifs_provider::TokenEndpointAuthMethod::None,
            refresh_token_rotates: true,
            extra_authorize_params: vec![],
            extra_token_params: vec![],
            inject_domains: vec!["example.com".to_owned()],
            inject_header_name: None,
            inject_value_prefix: "Bearer ".to_owned(),
        })],
    };
    write_provider_with_metadata(&paths, Some(&manifest), "github-real");

    let catalog = ProviderCatalog::for_providers(&paths.providers_dir);
    let mounts = mounts_for(&paths);
    let mount = catalog.load_mount_auth(&mounts, "example").unwrap();
    let (_request, target) = mount.oauth_request(None, &[]).unwrap();

    assert_eq!(target.primary_key().unwrap().provider_id(), "github-real");
}

#[test]
fn manual_code_input_accepts_redirect_url() {
    let code = manual_code_from_input("http://localhost/callback?code=abc&state=xyz").unwrap();
    assert_eq!(code.code, "abc");
    assert_eq!(code.state.secret(), "xyz");
}

fn fixture_paths(root: &Path) -> WorkspaceLayout {
    let paths = WorkspaceLayout::under_root(root);
    std::fs::create_dir_all(&paths.mounts_dir).unwrap();
    std::fs::create_dir_all(&paths.providers_dir).unwrap();
    paths
}

fn write_mount(paths: &WorkspaceLayout, name: &str, json: &str) {
    std::fs::write(paths.mounts_dir.join(format!("{name}.json")), json).unwrap();
}

fn write_provider(paths: &WorkspaceLayout, manifest: Option<&AuthManifest>) {
    write_provider_with_metadata(paths, manifest, "github");
}

fn write_provider_with_metadata(
    paths: &WorkspaceLayout,
    manifest: Option<&AuthManifest>,
    id: &str,
) {
    let mut metadata = serde_json::json!({
        "id": id,
        "displayName": id,
        "provider": "github.wasm",
        "defaultMount": "github",
        "capabilities": []
    });
    if let Some(manifest) = manifest {
        metadata["auth"] = auth_block_json(manifest);
    }
    let data = serde_json::to_vec(&metadata).unwrap();
    let mut wasm = b"\0asm\x01\0\0\0".to_vec();
    let mut section = Vec::new();
    push_uleb(
        omnifs_provider::PROVIDER_METADATA_SECTION_NAME.len(),
        &mut section,
    );
    section.extend_from_slice(omnifs_provider::PROVIDER_METADATA_SECTION_NAME.as_bytes());
    section.extend_from_slice(&data);
    wasm.push(0);
    push_uleb(section.len(), &mut wasm);
    wasm.extend(section);
    std::fs::write(paths.providers_dir.join("github.wasm"), wasm).unwrap();
}

fn auth_block_json(manifest: &AuthManifest) -> serde_json::Value {
    let inject = serde_json::json!({
        "domains": first_inject_domains(&manifest.schemes[0]),
        "header": first_inject_header(&manifest.schemes[0]),
        "prefix": first_inject_prefix(&manifest.schemes[0]),
    });
    let default = match &manifest.schemes[0] {
        AuthScheme::None => "default".to_owned(),
        AuthScheme::StaticToken(scheme) => scheme.key.clone(),
        AuthScheme::Oauth(scheme) => scheme.key.clone(),
    };
    let mut schemes = serde_json::Map::new();
    for scheme in &manifest.schemes {
        match scheme {
            AuthScheme::None => {},
            AuthScheme::StaticToken(static_token) => {
                schemes.insert(
                    static_token.key.clone(),
                    serde_json::json!({
                        "type": "staticToken",
                        "description": static_token.description,
                    }),
                );
            },
            AuthScheme::Oauth(oauth) => {
                schemes.insert(oauth.key.clone(), oauth_scheme_json(oauth));
            },
        }
    }
    serde_json::json!({
        "inject": inject,
        "default": default,
        "schemes": schemes,
    })
}

fn first_inject_domains(scheme: &AuthScheme) -> Vec<String> {
    match scheme {
        AuthScheme::None => Vec::new(),
        AuthScheme::StaticToken(scheme) => scheme.inject_domains.clone(),
        AuthScheme::Oauth(scheme) => scheme.inject_domains.clone(),
    }
}

fn first_inject_header(scheme: &AuthScheme) -> String {
    match scheme {
        AuthScheme::None => "Authorization".to_owned(),
        AuthScheme::StaticToken(scheme) => scheme
            .header_name
            .clone()
            .unwrap_or_else(|| "Authorization".to_owned()),
        AuthScheme::Oauth(scheme) => scheme
            .inject_header_name
            .clone()
            .unwrap_or_else(|| "Authorization".to_owned()),
    }
}

fn first_inject_prefix(scheme: &AuthScheme) -> String {
    match scheme {
        AuthScheme::None => String::new(),
        AuthScheme::StaticToken(scheme) => scheme.value_prefix.clone(),
        AuthScheme::Oauth(scheme) => scheme.inject_value_prefix.clone(),
    }
}

fn oauth_scheme_json(oauth: &OauthScheme) -> serde_json::Value {
    let flow = match &oauth.flow {
        OAuthFlow::DeviceCode(config) => serde_json::json!({
            "kind": "deviceCode",
            "authorizationEndpoint": oauth.authorization_endpoint,
            "deviceAuthorizationEndpoint": config.device_authorization_endpoint,
            "tokenEndpoint": oauth.token_endpoint,
        }),
        OAuthFlow::PkceManualCode(config) => serde_json::json!({
            "kind": "pkceManualCode",
            "authorizationEndpoint": oauth.authorization_endpoint,
            "tokenEndpoint": oauth.token_endpoint,
            "redirectUri": config.redirect_uri,
        }),
        OAuthFlow::PkceLoopback(config) => serde_json::json!({
            "kind": "pkceLoopback",
            "authorizationEndpoint": oauth.authorization_endpoint,
            "tokenEndpoint": oauth.token_endpoint,
            "redirectUriTemplate": config.redirect_uri_template,
        }),
        OAuthFlow::ClientSideToken(config) => serde_json::json!({
            "kind": "clientSideToken",
            "authorizationEndpoint": oauth.authorization_endpoint,
            "tokenEndpoint": oauth.token_endpoint,
            "redirectUriTemplate": config.redirect_uri_template,
        }),
    };
    serde_json::json!({
        "type": "oauth",
        "displayName": oauth.display_name,
        "clientId": oauth.default_client_id.clone().unwrap_or_else(|| "test-client".to_owned()),
        "scopes": oauth.default_scopes,
        "flow": flow,
    })
}

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
