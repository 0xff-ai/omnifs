#![allow(unsafe_code)]

use omnifs_creds::{CredentialEntry, CredentialId, CredentialStore, FileStore, MemoryStore};
use omnifs_host::auth::{AuthManager, RefreshOutcome};
use omnifs_host::cache::blobs::BlobCache;
use omnifs_host::config::{AuthConfig, OAuthMountConfig, StaticTokenConfig};
use omnifs_host::omnifs::provider::types as wit_types;
use omnifs_host::runtime::blob::{BlobExecutor, BlobLimits};
use omnifs_host::runtime::capability::{CapabilityChecker, CapabilityGrants};
use omnifs_host::runtime::http_stack::HttpStack;
use omnifs_mount_schema::{
    AuthManifest, AuthScheme, OAuthFlow, OauthScheme, PkceManualCodeConfig, StaticTokenScheme,
    TokenEndpointAuthMethod,
};
use secrecy::{ExposeSecret, SecretString};
use std::ffi::OsString;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::Duration;
use time::OffsetDateTime;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;

static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

struct ScopedEnvVar {
    _guard: MutexGuard<'static, ()>,
    key: String,
    original: Option<OsString>,
}

impl ScopedEnvVar {
    fn set(key: &str, value: &str) -> Self {
        let guard = ENV_LOCK.lock().unwrap();
        let original = std::env::var_os(key);
        unsafe { std::env::set_var(key, value) };
        Self {
            _guard: guard,
            key: key.to_string(),
            original,
        }
    }
}

impl Drop for ScopedEnvVar {
    fn drop(&mut self) {
        match &self.original {
            Some(value) => unsafe { std::env::set_var(&self.key, value) },
            None => unsafe { std::env::remove_var(&self.key) },
        }
    }
}

fn github_pat_manifest() -> AuthManifest {
    AuthManifest {
        schemes: vec![AuthScheme::StaticToken(StaticTokenScheme {
            key: "pat".to_string(),
            header_name: Some("Authorization".to_string()),
            value_prefix: "Bearer ".to_string(),
            description: "pat".to_string(),
            inject_domains: vec!["api.github.com".to_string()],
        })],
    }
}

fn github_pat_auth(token_env: Option<&str>, token_file: Option<&str>) -> AuthConfig {
    AuthConfig::StaticToken(StaticTokenConfig {
        scheme: Some("pat".to_string()),
        account: None,
        token_env: token_env.map(str::to_owned),
        token_file: token_file.map(str::to_owned),
    })
}

fn github_pat_manager(auth: AuthConfig) -> AuthManager {
    AuthManager::from_configs_and_manifest(&[auth], Some(&github_pat_manifest())).unwrap()
}

#[test]
fn test_static_token_env_injection() {
    let auth = github_pat_auth(Some("OMNIFS_TEST_TOKEN_AUTH"), None);
    let _env = ScopedEnvVar::set("OMNIFS_TEST_TOKEN_AUTH", "ghp_test123");
    let manager = github_pat_manager(auth);
    let headers = manager.headers_for_url("https://api.github.com/repos");
    assert_eq!(headers.len(), 1);
    assert_eq!(headers[0].0, "Authorization");
    assert_eq!(headers[0].1, "Bearer ghp_test123");
}

#[test]
fn test_no_injection_without_config() {
    let manager = AuthManager::none();
    let headers = manager.headers_for_url("https://api.github.com/repos");
    assert!(headers.is_empty());
}

#[test]
fn test_missing_env_var_returns_no_headers() {
    let auth = github_pat_auth(Some("DEFINITELY_NOT_SET_12345"), None);
    let manager = github_pat_manager(auth);
    let headers = manager.headers_for_url("https://api.github.com/repos");
    assert!(headers.is_empty());
    assert!(manager.requires_auth_for_url("https://api.github.com/repos"));
}

#[test]
fn test_static_token_injection_from_file() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = dir.path().join("github_token");
    std::fs::write(&token_file, "ghp_file_token\n").unwrap();
    let auth = github_pat_auth(None, Some(&token_file.display().to_string()));
    let manager = github_pat_manager(auth);
    let headers = manager.headers_for_url("https://api.github.com/repos");
    assert_eq!(headers.len(), 1);
    assert_eq!(headers[0].0, "Authorization");
    assert_eq!(headers[0].1, "Bearer ghp_file_token");
}

#[test]
fn test_token_file_takes_precedence_over_env() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = dir.path().join("github_token");
    std::fs::write(&token_file, "ghp_from_file").unwrap();
    let auth = github_pat_auth(
        Some("OMNIFS_TEST_TOKEN_AUTH_PREFERRED"),
        Some(&token_file.display().to_string()),
    );
    let _env = ScopedEnvVar::set("OMNIFS_TEST_TOKEN_AUTH_PREFERRED", "ghp_from_env");
    let manager = github_pat_manager(auth);
    let headers = manager.headers_for_url("https://api.github.com/repos");
    assert_eq!(headers[0].1, "Bearer ghp_from_file");
}

#[test]
fn test_missing_token_file_falls_back_to_env() {
    let dir = tempfile::tempdir().unwrap();
    let missing_token_file = dir.path().join("missing_token");
    let auth = github_pat_auth(
        Some("OMNIFS_TEST_TOKEN_AUTH_FALLBACK"),
        Some(&missing_token_file.display().to_string()),
    );
    let _env = ScopedEnvVar::set("OMNIFS_TEST_TOKEN_AUTH_FALLBACK", "ghp_from_env");
    let manager = github_pat_manager(auth);
    let headers = manager.headers_for_url("https://api.github.com/repos");
    assert_eq!(headers[0].1, "Bearer ghp_from_env");
}

#[test]
fn test_auth_manifest_backed_static_token_injection() {
    let auth = AuthConfig::StaticToken(StaticTokenConfig {
        scheme: Some("pat".to_string()),
        account: None,
        token_env: Some("OMNIFS_TEST_MANIFEST_TOKEN".to_string()),
        token_file: None,
    });
    let manifest = AuthManifest {
        schemes: vec![AuthScheme::StaticToken(StaticTokenScheme {
            key: "pat".to_string(),
            header_name: Some("X-Test-Token".to_string()),
            value_prefix: "token ".to_string(),
            description: "test token".to_string(),
            inject_domains: vec!["api.example.com".to_string()],
        })],
    };
    let _env = ScopedEnvVar::set("OMNIFS_TEST_MANIFEST_TOKEN", "secret");

    let manager = AuthManager::from_configs_and_manifest(&[auth], Some(&manifest)).unwrap();

    assert_eq!(
        manager.headers_for_url("https://api.example.com/repos"),
        vec![("X-Test-Token".to_string(), "token secret".to_string())]
    );
    assert!(manager.requires_auth_for_url("https://api.example.com/repos"));
    assert!(
        manager
            .headers_for_url("https://other.example.com/repos")
            .is_empty()
    );
    assert!(!manager.requires_auth_for_url("https://other.example.com/repos"));
}

#[test]
fn test_auth_manifest_backed_static_token_missing_credential_still_requires_auth() {
    let auth = AuthConfig::StaticToken(StaticTokenConfig {
        scheme: Some("pat".to_string()),
        account: None,
        token_env: Some("DEFINITELY_NOT_SET_MANIFEST_TOKEN".to_string()),
        token_file: None,
    });
    let manifest = AuthManifest {
        schemes: vec![AuthScheme::StaticToken(StaticTokenScheme {
            key: "pat".to_string(),
            header_name: None,
            value_prefix: "Bearer ".to_string(),
            description: "test token".to_string(),
            inject_domains: vec!["api.example.com".to_string()],
        })],
    };

    let manager = AuthManager::from_configs_and_manifest(&[auth], Some(&manifest)).unwrap();

    assert!(
        manager
            .headers_for_url("https://api.example.com/repos")
            .is_empty()
    );
    assert!(manager.requires_auth_for_url("https://api.example.com/repos"));
}

#[test]
fn test_provider_without_auth_manifest_behaves_as_no_auth() {
    let manager = AuthManager::from_configs_and_manifest(&[], None).unwrap();

    assert!(
        manager
            .headers_for_url("https://api.example.com/repos")
            .is_empty()
    );
    assert!(!manager.requires_auth_for_url("https://api.example.com/repos"));
}

#[tokio::test]
async fn refresh_without_applicable_strategy_reports_not_applicable() {
    let manager = AuthManager::none();

    assert_eq!(
        manager
            .refresh_for_url("https://api.example.com/repos")
            .await
            .unwrap(),
        RefreshOutcome::NotApplicable
    );
}

#[tokio::test]
async fn refresh_without_stored_oauth_credential_reports_no_credential() {
    let tokens = FakeTokenServer::start(false).await;
    let (auth, _store, _key) = oauth_manager(tokens.endpoint(), "localhost".to_string());

    assert_eq!(
        auth.refresh_for_url("https://localhost/resource")
            .await
            .unwrap(),
        RefreshOutcome::NoCredential
    );
    assert_eq!(tokens.refreshes(), 0);
}

#[tokio::test]
async fn test_execute_fetch_returns_denied_when_auth_is_required_but_missing() {
    // Create an AuthManager with a config that requires auth for api.github.com
    // but has no valid credential (env var doesn't exist). The injector should
    // exist (so requires_auth_for_url returns true) but have no header_value
    // (so headers_for_url returns empty).
    let auth = Arc::new(github_pat_manager(github_pat_auth(
        Some("DEFINITELY_NOT_SET_12345"),
        None,
    )));

    // Verify the setup: auth is required for this domain but no headers available
    assert!(auth.requires_auth_for_url("https://api.github.com/repos"));
    assert!(
        auth.headers_for_url("https://api.github.com/repos")
            .is_empty()
    );

    let capability = Arc::new(CapabilityChecker::new(CapabilityGrants {
        domains: vec!["api.github.com".to_string()],
        git_repos: Vec::new(),
        max_memory_mb: 64,
        needs_git: false,
        unix_sockets: Vec::new(),
    }));
    let stack = HttpStack::new(auth, capability).unwrap();

    let req = wit_types::HttpRequest {
        method: "GET".to_string(),
        url: "https://api.github.com/repos".to_string(),
        headers: Vec::new(),
        body: None,
    };
    match stack.fetch(&req, Duration::from_secs(30)).await {
        wit_types::CalloutResult::CalloutError(wit_types::CalloutError {
            kind: wit_types::ErrorKind::Denied,
            retryable: false,
            ..
        }) => {},
        other => panic!("expected denied error, got {other:?}"),
    }
}

#[tokio::test]
async fn oauth_401_refreshes_and_retries_once() {
    let tokens = FakeTokenServer::start(false).await;
    let api = FakeHttpsApiServer::start("Bearer access-refresh-1", "ok").await;
    let (auth, store, key) = oauth_manager(tokens.endpoint(), FakeHttpsApiServer::domain());
    seed_oauth(store.as_ref(), &key, "old-access", "refresh-1", 3600);
    auth.prepare_for_url(&api.url()).await.unwrap();

    let stack = HttpStack::with_https_client(
        auth,
        Arc::new(CapabilityChecker::new(CapabilityGrants {
            domains: vec![FakeHttpsApiServer::domain()],
            git_repos: Vec::new(),
            max_memory_mb: 64,
            needs_git: false,
            unix_sockets: Vec::new(),
        })),
        test_https_client(),
    );

    let req = wit_types::HttpRequest {
        method: "GET".to_string(),
        url: api.url(),
        headers: Vec::new(),
        body: None,
    };

    match stack.fetch(&req, Duration::from_secs(5)).await {
        wit_types::CalloutResult::HttpResponse(response) => {
            assert_eq!(response.status, 200);
            assert_eq!(response.body, b"ok");
        },
        other => panic!("expected successful response, got {other:?}"),
    }

    assert_eq!(api.calls(), 2);
    assert_eq!(tokens.refreshes(), 1);
    assert_eq!(
        store
            .get(&key)
            .unwrap()
            .unwrap()
            .access_token()
            .expose_secret(),
        "access-refresh-1"
    );
}

#[tokio::test]
async fn concurrent_oauth_refreshes_coalesce_inside_one_process() {
    let tokens = FakeTokenServer::start(false).await;
    let (auth, store, key) = oauth_manager(tokens.endpoint(), "localhost".to_string());
    seed_oauth(store.as_ref(), &key, "old-access", "refresh-1", 3600);
    auth.prepare_for_url("https://localhost/resource")
        .await
        .unwrap();

    let results = futures::future::join_all((0..8).map(|_| {
        let auth = Arc::clone(&auth);
        async move { auth.refresh_for_url("https://localhost/resource").await }
    }))
    .await;

    assert!(
        results
            .into_iter()
            .all(|result| result.unwrap() == RefreshOutcome::Refreshed)
    );
    assert_eq!(tokens.refreshes(), 1);
}

#[tokio::test]
async fn independent_oauth_strategies_share_refresh_lock() {
    let temp = tempfile::tempdir().unwrap();
    let store_path = temp.path().join("credentials.json");
    let store_lock = temp.path().join("credentials.json.lock");
    let refresh_lock = temp.path().join("credentials.lock");
    let key = CredentialId::new("test-provider", "oauth", "default").unwrap();
    FileStore::with_lock_path(&store_path, &store_lock)
        .put(
            &key,
            &oauth_entry(
                "old-access",
                "refresh-1",
                OffsetDateTime::now_utc() + time::Duration::hours(1),
            ),
        )
        .unwrap();

    let tokens = FakeTokenServer::start(false).await;
    let manifest = oauth_manifest(tokens.endpoint(), "localhost".to_string());
    let config = oauth_config();
    let make_auth = || {
        let store: Arc<dyn CredentialStore> =
            Arc::new(FileStore::with_lock_path(&store_path, &store_lock));
        AuthManager::from_configs_manifest_store_with_http(
            std::slice::from_ref(&config),
            Some(&manifest),
            "test-provider",
            store,
            refresh_lock.clone(),
            reqwest::Client::new(),
        )
        .unwrap()
    };
    let auth_a = Arc::new(make_auth());
    let auth_b = Arc::new(make_auth());
    auth_a
        .prepare_for_url("https://localhost/resource")
        .await
        .unwrap();
    auth_b
        .prepare_for_url("https://localhost/resource")
        .await
        .unwrap();

    let (left, right) = tokio::join!(
        auth_a.refresh_for_url("https://localhost/resource"),
        auth_b.refresh_for_url("https://localhost/resource")
    );

    assert_eq!(left.unwrap(), RefreshOutcome::Refreshed);
    assert_eq!(right.unwrap(), RefreshOutcome::Refreshed);
    assert_eq!(tokens.refreshes(), 1);
    assert_eq!(
        FileStore::with_lock_path(&store_path, &store_lock)
            .get(&key)
            .unwrap()
            .unwrap()
            .access_token()
            .expose_secret(),
        "access-refresh-1"
    );
}

#[tokio::test]
async fn fetch_blob_uses_same_oauth_retry_path() {
    let tokens = FakeTokenServer::start(false).await;
    let api = FakeHttpsApiServer::start("Bearer access-refresh-1", "blob-body").await;
    let (auth, store, key) = oauth_manager(tokens.endpoint(), FakeHttpsApiServer::domain());
    seed_oauth(store.as_ref(), &key, "old-access", "refresh-1", 3600);
    auth.prepare_for_url(&api.url()).await.unwrap();

    let stack = Arc::new(HttpStack::with_https_client(
        auth,
        Arc::new(CapabilityChecker::new(CapabilityGrants {
            domains: vec![FakeHttpsApiServer::domain()],
            git_repos: Vec::new(),
            max_memory_mb: 64,
            needs_git: false,
            unix_sockets: Vec::new(),
        })),
        test_https_client(),
    ));
    let temp = tempfile::tempdir().unwrap();
    let executor = BlobExecutor::new(
        stack,
        Arc::new(BlobCache::new(temp.path().to_path_buf())),
        BlobLimits::default(),
    );

    let result = executor
        .fetch(&wit_types::BlobFetchRequest {
            method: "GET".to_string(),
            url: api.url(),
            headers: Vec::new(),
            body: None,
            cache_key: "oauth-blob".to_string(),
        })
        .await;

    match result {
        wit_types::CalloutResult::BlobFetched(blob) => {
            assert_eq!(blob.status, 200);
            assert_eq!(blob.size, 9);
        },
        other => panic!("expected blob fetch, got {other:?}"),
    }
    assert_eq!(api.calls(), 2);
    assert_eq!(tokens.refreshes(), 1);
}

#[tokio::test]
async fn oauth_refresh_failure_surfaces_denied_and_clears_store() {
    let tokens = FakeTokenServer::start(true).await;
    let api = FakeHttpsApiServer::start("Bearer never-used", "ok").await;
    let (auth, store, key) = oauth_manager(tokens.endpoint(), FakeHttpsApiServer::domain());
    seed_oauth(store.as_ref(), &key, "old-access", "refresh-1", 3600);
    auth.prepare_for_url(&api.url()).await.unwrap();

    let stack = HttpStack::with_https_client(
        auth,
        Arc::new(CapabilityChecker::new(CapabilityGrants {
            domains: vec![FakeHttpsApiServer::domain()],
            git_repos: Vec::new(),
            max_memory_mb: 64,
            needs_git: false,
            unix_sockets: Vec::new(),
        })),
        test_https_client(),
    );

    let req = wit_types::HttpRequest {
        method: "GET".to_string(),
        url: api.url(),
        headers: Vec::new(),
        body: None,
    };

    match stack.fetch(&req, Duration::from_secs(5)).await {
        wit_types::CalloutResult::CalloutError(wit_types::CalloutError {
            kind: wit_types::ErrorKind::Denied,
            retryable: false,
            ..
        }) => {},
        other => panic!("expected denied refresh failure, got {other:?}"),
    }

    assert_eq!(api.calls(), 1);
    assert_eq!(tokens.refreshes(), 0);
    assert!(store.get(&key).unwrap().is_none());
}

#[tokio::test]
async fn oauth_config_client_id_overrides_missing_manifest_default_for_refresh() {
    let tokens = FakeTokenServer::start_with_expected_client_id(false, Some("byo-client")).await;
    let store: Arc<dyn CredentialStore> = Arc::new(MemoryStore::default());
    let key = CredentialId::new("test-provider", "oauth", "default").unwrap();
    store
        .put(
            &key,
            &oauth_entry(
                "old-access",
                "refresh-1",
                OffsetDateTime::now_utc() + time::Duration::seconds(3600),
            ),
        )
        .unwrap();

    let mut config = oauth_config();
    let AuthConfig::OAuth(oauth) = &mut config else {
        panic!("expected oauth config");
    };
    oauth.client_id = Some("byo-client".to_string());
    let mut manifest = oauth_manifest(tokens.endpoint(), "localhost".to_string());
    let AuthScheme::Oauth(scheme) = &mut manifest.schemes[0] else {
        panic!("expected oauth scheme");
    };
    scheme.default_client_id = None;

    let auth = AuthManager::from_configs_manifest_store_with_http(
        &[config],
        Some(&manifest),
        "test-provider",
        Arc::clone(&store),
        tempfile::tempdir().unwrap().path().join("credentials.lock"),
        reqwest::Client::new(),
    )
    .unwrap();

    assert_eq!(
        auth.refresh_for_url("https://localhost/resource")
            .await
            .unwrap(),
        RefreshOutcome::Refreshed
    );
    assert_eq!(tokens.refreshes(), 1);
}

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

fn oauth_manifest(token_endpoint: String, inject_domain: String) -> AuthManifest {
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
            inject_domains: vec![inject_domain],
            inject_header_name: None,
            inject_value_prefix: "Bearer ".to_string(),
        })],
    }
}

fn oauth_manager(
    token_endpoint: String,
    inject_domain: String,
) -> (Arc<AuthManager>, Arc<dyn CredentialStore>, CredentialId) {
    let store: Arc<dyn CredentialStore> = Arc::new(MemoryStore::default());
    let key = CredentialId::new("test-provider", "oauth", "default").unwrap();
    let auth = AuthManager::from_configs_manifest_store_with_http(
        &[oauth_config()],
        Some(&oauth_manifest(token_endpoint, inject_domain)),
        "test-provider",
        Arc::clone(&store),
        tempfile::tempdir().unwrap().path().join("credentials.lock"),
        reqwest::Client::new(),
    )
    .unwrap();
    (Arc::new(auth), store, key)
}

fn seed_oauth(
    store: &dyn CredentialStore,
    key: &CredentialId,
    access_token: &str,
    refresh_token: &str,
    expires_in_seconds: i64,
) {
    let expires_at = OffsetDateTime::now_utc() + time::Duration::seconds(expires_in_seconds);
    store
        .put(key, &oauth_entry(access_token, refresh_token, expires_at))
        .unwrap();
}

fn oauth_entry(
    access_token: &str,
    refresh_token: &str,
    expires_at: OffsetDateTime,
) -> CredentialEntry {
    CredentialEntry::oauth(
        SecretString::from(access_token.to_string()),
        Some(SecretString::from(refresh_token.to_string())),
        Some(expires_at),
        "Bearer",
        vec!["read".to_string()],
        OffsetDateTime::now_utc(),
    )
}

fn test_https_client() -> reqwest::Client {
    reqwest::ClientBuilder::new()
        .danger_accept_invalid_certs(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

#[derive(Clone)]
struct FakeTokenServer {
    endpoint: String,
    refreshes: Arc<AtomicUsize>,
    expected_client_id: Option<String>,
}

impl FakeTokenServer {
    async fn start(fail: bool) -> Self {
        Self::start_with_expected_client_id(fail, None).await
    }

    async fn start_with_expected_client_id(fail: bool, expected_client_id: Option<&str>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = Self {
            endpoint: format!("http://{addr}/token"),
            refreshes: Arc::new(AtomicUsize::new(0)),
            expected_client_id: expected_client_id.map(str::to_owned),
        };
        let task_server = server.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let task_server = task_server.clone();
                tokio::spawn(async move {
                    let request = read_http_request(&mut stream).await;
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
                    if let Some(expected_client_id) = &task_server.expected_client_id {
                        assert_eq!(
                            params.get("client_id").map(String::as_str),
                            Some(expected_client_id.as_str())
                        );
                    }
                    if fail {
                        let body = r#"{"error":"invalid_grant","error_description":"revoked"}"#;
                        write_http_response(
                            &mut stream,
                            "400 Bad Request",
                            "application/json",
                            body,
                        )
                        .await;
                        return;
                    }
                    let id = task_server.refreshes.fetch_add(1, Ordering::SeqCst) + 1;
                    let body = serde_json::json!({
                        "access_token": format!("access-refresh-{id}"),
                        "refresh_token": format!("refresh-rotated-{id}"),
                        "expires_in": 3600,
                        "token_type": "Bearer",
                        "scope": "read",
                    })
                    .to_string();
                    write_http_response(&mut stream, "200 OK", "application/json", &body).await;
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

#[derive(Clone)]
struct FakeHttpsApiServer {
    url: String,
    calls: Arc<AtomicUsize>,
}

impl FakeHttpsApiServer {
    async fn start(success_authorization: &'static str, success_body: &'static str) -> Self {
        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let cert_der = cert.cert.der().clone();
        let key_der = tokio_rustls::rustls::pki_types::PrivateKeyDer::Pkcs8(
            tokio_rustls::rustls::pki_types::PrivatePkcs8KeyDer::from(
                cert.key_pair.serialize_der(),
            ),
        );
        let config = tokio_rustls::rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der], key_der)
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let server = Self {
            url: format!("https://localhost:{}/resource", addr.port()),
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let task_server = server.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let acceptor = acceptor.clone();
                let task_server = task_server.clone();
                tokio::spawn(async move {
                    let Ok(mut stream) = acceptor.accept(stream).await else {
                        return;
                    };
                    let request = read_http_request(&mut stream).await;
                    task_server.calls.fetch_add(1, Ordering::SeqCst);
                    let authorization = authorization_header(&request);
                    if authorization.as_deref() == Some(success_authorization) {
                        write_http_response(&mut stream, "200 OK", "text/plain", success_body)
                            .await;
                    } else {
                        let response = concat!(
                            "HTTP/1.1 401 Unauthorized\r\n",
                            "www-authenticate: Bearer error=\"invalid_token\"\r\n",
                            "content-length: 0\r\n",
                            "connection: close\r\n",
                            "\r\n"
                        );
                        stream.write_all(response.as_bytes()).await.unwrap();
                    }
                });
            }
        });
        server
    }

    fn url(&self) -> String {
        self.url.clone()
    }

    fn domain() -> String {
        "localhost".to_string()
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

async fn read_http_request<R>(stream: &mut R) -> String
where
    R: AsyncRead + Unpin,
{
    let mut buf = vec![0; 8192];
    let read = stream.read(&mut buf).await.unwrap();
    String::from_utf8_lossy(&buf[..read]).into_owned()
}

async fn write_http_response<W>(stream: &mut W, status: &str, content_type: &str, body: &str)
where
    W: AsyncWrite + Unpin,
{
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await.unwrap();
}

fn authorization_header(request: &str) -> Option<String> {
    request.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("authorization")
            .then(|| value.trim().to_string())
    })
}
