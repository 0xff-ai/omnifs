//! Characterization: `AuthManager` OAuth refresh behavior.
//!
//! N2 freezes CURRENT behavior at the `AuthManager` contract level (the surface
//! A1/A3 refactor into a `CredentialService`). `auth_test.rs` already exercises
//! the same three behaviors end-to-end through `HttpStack` + a live HTTPS API;
//! this file pins them directly on the manager so the coming refactor has one
//! focused home:
//!
//!   (a) preparing a request whose credential expires inside the 60s refresh
//!       window triggers a synchronous refresh (and a fresh credential does not);
//!   (b) a 401 (and a 403 carrying `WWW-Authenticate: ... invalid_token`) is
//!       classified as refreshable, and a forced refresh rotates the token once;
//!   (c) an `invalid_grant` refresh DELETES the stored credential.
//!
//! Assertion (c) is TODAY's behavior. A3 intentionally flips it to a
//! `NeedsConsent` transition that keeps the stored secret; A3 must update this
//! test when it does.
//!
//! Harness note: the step named `omnifs-auth`'s `FakeAuthServer`, but that type
//! is `pub(super)` inside `omnifs-auth`'s `client` module and unreachable from an
//! `omnifs-host` integration test. This file uses a local fake token server, the
//! same idiom `auth_test.rs` uses for the host-side refresh tests.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use omnifs_core::CredentialId;
use omnifs_creds::{CredentialEntry, CredentialStore, MemoryStore};
use omnifs_host::auth::{AuthManager, RefreshOutcome};
use omnifs_mount::{Auth as AuthConfig, OAuth as OAuthMountConfig};
use omnifs_provider::{
    AuthManifest, AuthScheme, OAuthFlow, OauthScheme, PkceManualCodeConfig, TokenEndpointAuthMethod,
};
use secrecy::{ExposeSecret, SecretString};
use time::OffsetDateTime;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const RESOURCE_URL: &str = "https://localhost/resource";

/// Preparing a request whose OAuth credential expires within the 60s refresh
/// window triggers a synchronous refresh against the token endpoint.
#[tokio::test]
async fn prepare_inside_refresh_window_refreshes_synchronously() {
    let tokens = FakeTokenServer::start(false).await;
    let (auth, store, key) = oauth_manager(tokens.endpoint());
    // 30s to expiry: inside the 60s window, so not "fresh".
    seed_oauth(store.as_ref(), &key, "old-access", "refresh-1", 30);

    auth.prepare_for_url(RESOURCE_URL).await.unwrap();

    assert_eq!(
        tokens.refreshes(),
        1,
        "a near-expiry credential refreshes on prepare"
    );
    assert_eq!(
        store
            .get(&key)
            .unwrap()
            .unwrap()
            .access_token()
            .expose_secret(),
        "access-refresh-1",
        "the refreshed access token is persisted",
    );
}

/// Preparing a request whose OAuth credential is comfortably valid does NOT
/// refresh: the 60s window gates the proactive refresh.
#[tokio::test]
async fn prepare_outside_refresh_window_does_not_refresh() {
    let tokens = FakeTokenServer::start(false).await;
    let (auth, store, key) = oauth_manager(tokens.endpoint());
    // 1h to expiry: comfortably fresh.
    seed_oauth(store.as_ref(), &key, "old-access", "refresh-1", 3600);

    auth.prepare_for_url(RESOURCE_URL).await.unwrap();

    assert_eq!(tokens.refreshes(), 0, "a fresh credential is not refreshed");
    assert_eq!(
        store
            .get(&key)
            .unwrap()
            .unwrap()
            .access_token()
            .expose_secret(),
        "old-access",
        "the stored credential is left untouched",
    );
}

/// A 401 (and a 403 carrying an `invalid_token` bearer challenge) is classified
/// as refreshable; a plain 403 and a 500 are not. A forced refresh then rotates
/// the token exactly once.
#[tokio::test]
async fn response_401_is_refreshable_and_forced_refresh_rotates_once() {
    use reqwest::StatusCode;

    let tokens = FakeTokenServer::start(false).await;
    let (auth, store, key) = oauth_manager(tokens.endpoint());
    seed_oauth(store.as_ref(), &key, "old-access", "refresh-1", 3600);
    // Load `current` from the store so a forced refresh actually hits the token
    // endpoint (rather than adopting the store entry it already matches).
    auth.prepare_for_url(RESOURCE_URL).await.unwrap();
    assert_eq!(
        tokens.refreshes(),
        0,
        "prepare with a fresh token does not refresh"
    );

    let empty = reqwest::header::HeaderMap::new();
    let mut invalid_token = reqwest::header::HeaderMap::new();
    invalid_token.insert(
        reqwest::header::WWW_AUTHENTICATE,
        reqwest::header::HeaderValue::from_static("Bearer error=\"invalid_token\""),
    );

    assert!(
        auth.should_refresh_for_response(RESOURCE_URL, StatusCode::UNAUTHORIZED, &empty),
        "401 is refreshable"
    );
    assert!(
        auth.should_refresh_for_response(RESOURCE_URL, StatusCode::FORBIDDEN, &invalid_token),
        "403 + invalid_token bearer challenge is refreshable"
    );
    assert!(
        !auth.should_refresh_for_response(RESOURCE_URL, StatusCode::FORBIDDEN, &empty),
        "a plain 403 is not refreshable"
    );
    assert!(
        !auth.should_refresh_for_response(RESOURCE_URL, StatusCode::INTERNAL_SERVER_ERROR, &empty),
        "a 500 is not refreshable"
    );

    assert_eq!(
        auth.refresh_for_url(RESOURCE_URL).await.unwrap(),
        RefreshOutcome::Refreshed
    );
    assert_eq!(
        tokens.refreshes(),
        1,
        "the forced refresh hit the token endpoint once"
    );
    assert_eq!(
        store
            .get(&key)
            .unwrap()
            .unwrap()
            .access_token()
            .expose_secret(),
        "access-refresh-1",
    );
}

/// An `invalid_grant` response to a refresh DELETES the stored credential.
///
/// TODAY's behavior. A3 replaces the deletion with a `NeedsConsent` transition
/// that preserves the stored secret; A3 must update this assertion when it does.
#[tokio::test]
async fn invalid_grant_refresh_deletes_stored_credential() {
    let tokens = FakeTokenServer::start(true).await;
    let (auth, store, key) = oauth_manager(tokens.endpoint());
    seed_oauth(store.as_ref(), &key, "old-access", "refresh-1", 3600);
    // Load `current` so the forced refresh reaches the token endpoint.
    auth.prepare_for_url(RESOURCE_URL).await.unwrap();

    let result = auth.refresh_for_url(RESOURCE_URL).await;
    assert!(
        result.is_err(),
        "an invalid_grant refresh surfaces an error"
    );

    assert!(
        store.get(&key).unwrap().is_none(),
        "invalid_grant deletes the stored credential (A3 changes this)"
    );
}

// ---------------------------------------------------------------------------
// Local harness (mirrors auth_test.rs; FakeAuthServer is unreachable from here).
// ---------------------------------------------------------------------------

fn oauth_config() -> AuthConfig {
    AuthConfig::OAuth(OAuthMountConfig {
        scheme: Some("oauth".to_string()),
        account: Some("default".to_string()),
        domain: None,
        header: None,
        client_id: None,
        client_secret_env: None,
        client_secret_file: None,
        redirect_uri: None,
        scopes: None,
    })
}

fn oauth_manifest(token_endpoint: String) -> AuthManifest {
    AuthManifest {
        schemes: vec![AuthScheme::Oauth(OauthScheme {
            key: "oauth".to_string(),
            display_name: "test oauth".to_string(),
            authorization_endpoint: "http://localhost/authorize".to_string(),
            token_endpoint,
            revocation_endpoint: None,
            default_client_id: Some("client-id".to_string()),
            default_scopes: vec!["read".to_string()],
            flow: OAuthFlow::PkceManualCode(PkceManualCodeConfig {
                redirect_uri: "http://localhost/callback".to_string(),
            }),
            token_endpoint_auth: TokenEndpointAuthMethod::None,
            refresh_token_rotates: true,
            extra_authorize_params: vec![],
            extra_token_params: vec![],
            inject_domains: vec!["localhost".to_string()],
            inject_header_name: None,
            inject_value_prefix: "Bearer ".to_string(),
        })],
    }
}

fn oauth_manager(token_endpoint: String) -> (AuthManager, Arc<dyn CredentialStore>, CredentialId) {
    let store: Arc<dyn CredentialStore> = Arc::new(MemoryStore::default());
    let key = CredentialId::new("test-provider", "oauth", "default").unwrap();
    let auth = AuthManager::from_configs_manifest_store_with_http(
        &[oauth_config()],
        Some(&oauth_manifest(token_endpoint)),
        "test-provider",
        Arc::clone(&store),
        reqwest_oauth2::Client::new(),
    )
    .unwrap();
    (auth, store, key)
}

fn seed_oauth(
    store: &dyn CredentialStore,
    key: &CredentialId,
    access_token: &str,
    refresh_token: &str,
    expires_in_seconds: i64,
) {
    let expires_at = OffsetDateTime::now_utc() + time::Duration::seconds(expires_in_seconds);
    let entry = CredentialEntry::oauth(
        SecretString::from(access_token.to_string()),
        Some(SecretString::from(refresh_token.to_string())),
        Some(expires_at),
        "Bearer",
        vec!["read".to_string()],
        OffsetDateTime::now_utc(),
    );
    store.put(key, &entry).unwrap();
}

#[derive(Clone)]
struct FakeTokenServer {
    endpoint: String,
    refreshes: Arc<AtomicUsize>,
}

impl FakeTokenServer {
    /// `fail = true` makes every refresh return `invalid_grant`.
    async fn start(fail: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = Self {
            endpoint: format!("http://{addr}/token"),
            refreshes: Arc::new(AtomicUsize::new(0)),
        };
        let task = server.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let task = task.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0; 8192];
                    let read = stream.read(&mut buf).await.unwrap();
                    let request = String::from_utf8_lossy(&buf[..read]);
                    assert!(request.starts_with("POST /token "));
                    let body = request.split("\r\n\r\n").nth(1).unwrap_or_default();
                    let params: std::collections::HashMap<String, String> =
                        url::form_urlencoded::parse(body.as_bytes())
                            .into_owned()
                            .collect();
                    assert_eq!(
                        params.get("grant_type").map(String::as_str),
                        Some("refresh_token")
                    );
                    if fail {
                        let body = r#"{"error":"invalid_grant","error_description":"revoked"}"#;
                        write_response(&mut stream, "400 Bad Request", body).await;
                        return;
                    }
                    let id = task.refreshes.fetch_add(1, Ordering::SeqCst) + 1;
                    let body = serde_json::json!({
                        "access_token": format!("access-refresh-{id}"),
                        "refresh_token": format!("refresh-rotated-{id}"),
                        "expires_in": 3600,
                        "token_type": "Bearer",
                        "scope": "read",
                    })
                    .to_string();
                    write_response(&mut stream, "200 OK", &body).await;
                });
            }
        });
        server
    }

    fn endpoint(&self) -> String {
        self.endpoint.clone()
    }

    fn refreshes(&self) -> usize {
        self.refreshes.load(Ordering::SeqCst)
    }
}

async fn write_response(stream: &mut tokio::net::TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await.unwrap();
}
