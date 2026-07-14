//! `CredentialService::spawn_refresh_loop` proactively refreshes an OAuth
//! credential before it enters the refresh window, driven entirely by its own
//! timer: no request-path call (`authorization`/`cached_authorization`) is
//! what triggers the refresh in this test.
//!
//! Harness note: mirrors the local fake token server in
//! `auth_refresh_characterize.rs` (`FakeAuthServer` is unreachable from an
//! `omnifs-engine` integration test; see that file's header).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration as StdDuration;

use omnifs_auth::{CredentialHealth, CredentialService, OAuthClient, OAuthRequest, REFRESH_WINDOW};
use omnifs_workspace::authn::{
    CredentialId, OAuthFlow, OauthScheme, PkceManualCodeConfig, TokenEndpointAuthMethod,
};
use omnifs_workspace::creds::{CredentialEntry, CredentialStore, MemoryStore};
use secrecy::{ExposeSecret, SecretString};
use time::OffsetDateTime;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn refresh_loop_refreshes_before_the_request_path_ever_asks() {
    let tokens = FakeTokenServer::start().await;
    let store: Arc<dyn CredentialStore> = Arc::new(MemoryStore::default());
    let key = CredentialId::new("test-provider", "oauth", "default").unwrap();
    // 2x the refresh window: the loop's deadline (expires_at - REFRESH_WINDOW)
    // lands one window from now.
    #[allow(clippy::cast_possible_wrap)]
    let expires_in_seconds = 2 * REFRESH_WINDOW.as_secs() as i64;
    seed_oauth(
        store.as_ref(),
        &key,
        "old-access",
        "refresh-1",
        expires_in_seconds,
    );

    let service = Arc::new(CredentialService::new(
        store.clone(),
        OAuthClient::new().expect("build oauth client"),
    ));
    service
        .bind_oauth(key.clone(), oauth_request(tokens.endpoint()))
        .unwrap();

    let handle = service.spawn_refresh_loop();

    // Give the spawned task a chance to reach its first await point (sleeping
    // on the computed deadline), then confirm nothing has happened yet: no
    // request-path call is made here, only the loop's own registration.
    tokio::task::yield_now().await;
    assert_eq!(
        tokens.refreshes(),
        0,
        "no refresh before the loop's own deadline elapses"
    );

    // Comfortably past REFRESH_WINDOW plus the loop's max 10% jitter.
    tokio::time::advance(REFRESH_WINDOW + StdDuration::from_secs(10)).await;

    // `advance` only fires the virtual timer; it does not drive the real
    // (unpaused) TCP round-trip the woken task then makes to the fake token
    // endpoint. A `spawn_blocking` real sleep forces the executor to park and
    // service that I/O to completion instead of busy-polling a task that
    // hasn't been woken by the reactor yet.
    tokio::task::spawn_blocking(|| std::thread::sleep(std::time::Duration::from_millis(200)))
        .await
        .unwrap();

    assert_eq!(
        tokens.refreshes(),
        1,
        "the loop refreshed the credential without any inbound request"
    );
    let stored = store
        .get(&key)
        .unwrap()
        .expect("credential still stored after refresh");
    assert_eq!(
        stored.access_token().expose_secret(),
        "access-refresh-1",
        "the refreshed access token is persisted"
    );

    let health = service.health();
    let status = health
        .iter()
        .find(|status| status.id == key)
        .expect("bound credential reports health");
    assert_eq!(status.health, CredentialHealth::Ready);

    handle.abort();
}

// ---------------------------------------------------------------------------
// Local harness (mirrors auth_refresh_characterize.rs; FakeAuthServer is
// unreachable from here).
// ---------------------------------------------------------------------------

fn oauth_scheme(token_endpoint: String) -> OauthScheme {
    OauthScheme {
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
    }
}

fn oauth_request(token_endpoint: String) -> OAuthRequest {
    OAuthRequest::new(oauth_scheme(token_endpoint))
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
    async fn start() -> Self {
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
                    let params: HashMap<String, String> =
                        url::form_urlencoded::parse(body.as_bytes())
                            .into_owned()
                            .collect();
                    assert_eq!(
                        params.get("grant_type").map(String::as_str),
                        Some("refresh_token")
                    );
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
