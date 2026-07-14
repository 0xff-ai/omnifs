//! Characterization: mount-owned OAuth binding refresh behavior.
//!
//! `auth_test.rs` exercises these behaviors end to end through `HttpStack` and a
//! live HTTPS API; this file exercises the same contract directly on the binding:
//!
//!   (a) authorizing a request whose credential expires inside the 60s refresh
//!       window triggers a synchronous refresh (and a fresh credential does not);
//!   (b) a 401 (and a 403 carrying `WWW-Authenticate: ... invalid_token`) is
//!       reported to the auth service, which refreshes and rotates the token
//!       once;
//!   (c) an `invalid_grant` refresh marks the credential as `NeedsConsent` while
//!       keeping the stored entry.
//!
//! The `NeedsConsent` transition is fail-closed while retaining the stored
//! secret for diagnostics. This integration test uses a local fake token server
//! because `omnifs-auth`'s `FakeAuthServer` is private to its `client` module.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use omnifs_engine::test_support::auth::{RefreshOutcome, binding_with_store_and_http};
use omnifs_workspace::authn::CredentialId;
use omnifs_workspace::authn::{
    AuthManifest, AuthScheme, OAuthFlow, OauthScheme, PkceManualCodeConfig, TokenEndpointAuthMethod,
};
use omnifs_workspace::creds::{CredentialEntry, CredentialStore, MemoryStore};
use omnifs_workspace::mounts::{Auth as AuthConfig, OAuth as OAuthMountConfig};
use secrecy::{ExposeSecret, SecretString};
use time::OffsetDateTime;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const RESOURCE_URL: &str = "https://localhost/resource";

/// Authorizing a request whose OAuth credential expires within the 60s refresh
/// window triggers a synchronous refresh against the token endpoint.
#[tokio::test]
async fn authorization_inside_refresh_window_refreshes_synchronously() {
    let tokens = FakeTokenServer::start(false).await;
    let (auth, store, key) = oauth_binding_expiring(tokens.endpoint());
    // 30s to expiry: inside the 60s window, so not "fresh".
    seed_oauth(store.as_ref(), &key, "old-access", "refresh-1", 30);

    auth.authorization_for(RESOURCE_URL)
        .await
        .unwrap()
        .expect("oauth authorization header");

    assert_eq!(
        tokens.refreshes(),
        1,
        "a near-expiry credential refreshes on authorization"
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

/// Authorizing a request whose OAuth credential is comfortably valid does NOT
/// refresh: the 60s window gates the on-demand refresh.
#[tokio::test]
async fn authorization_outside_refresh_window_does_not_refresh() {
    let tokens = FakeTokenServer::start(false).await;
    let (auth, store, key) = oauth_binding(tokens.endpoint());
    // 1h to expiry: comfortably fresh.
    seed_oauth(store.as_ref(), &key, "old-access", "refresh-1", 3600);

    auth.authorization_for(RESOURCE_URL)
        .await
        .unwrap()
        .expect("oauth authorization header");

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

/// A 401 (and a 403 carrying an `invalid_token` bearer challenge) is reported
/// to the auth service as refreshable; a plain 403 and a 500 are not. A
/// refreshable rejection rotates the token exactly once.
#[tokio::test]
async fn response_401_is_refreshable_and_forced_refresh_rotates_once() {
    let tokens = FakeTokenServer::start(false).await;
    let (auth, store, key) = oauth_binding(tokens.endpoint());
    seed_oauth(store.as_ref(), &key, "old-access", "refresh-1", 3600);
    // Load `current` from the store so a forced refresh actually hits the token
    // endpoint (rather than adopting the store entry it already matches).
    auth.authorization_for(RESOURCE_URL)
        .await
        .unwrap()
        .expect("oauth authorization header");
    assert_eq!(
        tokens.refreshes(),
        0,
        "authorization with a fresh token does not refresh"
    );

    assert_eq!(
        auth.report_rejected_for_response(RESOURCE_URL, 401, None)
            .await,
        RefreshOutcome::Refreshed,
        "401 is refreshable"
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

    let tokens = FakeTokenServer::start(false).await;
    let (auth, store, key) = oauth_binding(tokens.endpoint());
    seed_oauth(store.as_ref(), &key, "old-access", "refresh-1", 3600);
    auth.authorization_for(RESOURCE_URL)
        .await
        .unwrap()
        .expect("oauth authorization header");
    assert_eq!(
        auth.report_rejected_for_response(
            RESOURCE_URL,
            403,
            Some("Bearer error=\"invalid_token\"".to_owned())
        )
        .await,
        RefreshOutcome::Refreshed,
        "403 + invalid_token bearer challenge is refreshable"
    );
    assert_eq!(tokens.refreshes(), 1);

    let tokens = FakeTokenServer::start(false).await;
    let (auth, store, key) = oauth_binding(tokens.endpoint());
    seed_oauth(store.as_ref(), &key, "old-access", "refresh-1", 3600);
    auth.authorization_for(RESOURCE_URL)
        .await
        .unwrap()
        .expect("oauth authorization header");
    assert_eq!(
        auth.report_rejected_for_response(RESOURCE_URL, 403, None)
            .await,
        RefreshOutcome::NotApplicable,
        "a plain 403 is not refreshable"
    );
    assert_eq!(
        auth.report_rejected_for_response(RESOURCE_URL, 500, None)
            .await,
        RefreshOutcome::NotApplicable,
        "a 500 is not refreshable"
    );
    assert_eq!(tokens.refreshes(), 0);
}

/// An `invalid_grant` response to a refresh keeps the stored credential for
/// diagnostics and marks it `NeedsConsent`, so later authorization fails closed.
#[tokio::test]
async fn invalid_grant_refresh_needs_consent_and_keeps_stored_credential() {
    let tokens = FakeTokenServer::start(true).await;
    let (auth, store, key) = oauth_binding(tokens.endpoint());
    seed_oauth(store.as_ref(), &key, "old-access", "refresh-1", 3600);
    // Load `current` so the forced refresh reaches the token endpoint.
    auth.authorization_for(RESOURCE_URL)
        .await
        .unwrap()
        .expect("oauth authorization header");

    let result = auth
        .report_rejected_for_response(RESOURCE_URL, 401, None)
        .await;
    assert!(
        matches!(result, RefreshOutcome::RefreshFailed(_)),
        "an invalid_grant refresh surfaces an error"
    );

    assert!(
        store.get(&key).unwrap().is_some(),
        "invalid_grant preserves the stored credential for diagnostics"
    );
    let err = auth.authorization_for(RESOURCE_URL).await.unwrap_err();
    assert!(
        err.to_string().contains("needs re-authentication"),
        "NeedsConsent credentials fail closed"
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

fn oauth_binding(
    token_endpoint: String,
) -> (
    Arc<omnifs_auth::AuthBinding>,
    Arc<dyn CredentialStore>,
    CredentialId,
) {
    let store: Arc<dyn CredentialStore> = Arc::new(MemoryStore::default());
    let key = CredentialId::new("test-provider", "oauth", "default").unwrap();
    seed_oauth(&*store, &key, "old-access", "refresh-1", 3600);
    let config = oauth_config();
    let auth = binding_with_store_and_http(
        Some(&config),
        Some(&oauth_manifest(token_endpoint)),
        "test-provider",
        Arc::clone(&store),
        reqwest_oauth2::Client::new(),
    );
    (auth.expect("configured auth"), store, key)
}

fn oauth_binding_expiring(
    token_endpoint: String,
) -> (
    Arc<omnifs_auth::AuthBinding>,
    Arc<dyn CredentialStore>,
    CredentialId,
) {
    let store: Arc<dyn CredentialStore> = Arc::new(MemoryStore::default());
    let key = CredentialId::new("test-provider", "oauth", "default").unwrap();
    seed_oauth(&*store, &key, "old-access", "refresh-1", 30);
    let config = oauth_config();
    let auth = binding_with_store_and_http(
        Some(&config),
        Some(&oauth_manifest(token_endpoint)),
        "test-provider",
        Arc::clone(&store),
        reqwest_oauth2::Client::new(),
    );
    (auth.expect("configured auth"), store, key)
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
