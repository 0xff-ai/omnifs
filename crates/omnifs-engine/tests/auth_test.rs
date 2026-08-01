use omnifs_auth::{
    AuthBinding, AuthManifest, AuthScheme, CredentialId, CredentialService, OAuthClient, OAuthFlow,
    OAuthRequest, OAuthRuntimeOverrides, OauthScheme, PkceManualCodeConfig, RefreshCandidate,
    RefreshOutcome, RefreshPersistError, RefreshPersistence, RefreshSink, StaticTokenScheme,
    TokenEndpointAuthMethod,
};
use omnifs_auth::{CredentialEntry, DurableCredentialSnapshot};
use omnifs_core::CredentialVersion;
use omnifs_engine::test_support::authority::RuntimeAuthority;
use omnifs_engine::test_support::blob::{BlobExecutor, BlobLimits};
use omnifs_engine::test_support::cache::{Caches, mount};
use omnifs_engine::test_support::http::HttpStack;
use omnifs_wit::host::types as wit_types;
use secrecy::{ExposeSecret, SecretString};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use time::OffsetDateTime;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;

fn github_pat_manifest() -> AuthManifest {
    AuthManifest {
        schemes: vec![AuthScheme::StaticToken(StaticTokenScheme {
            key: "pat".to_string(),
            header_name: Some("Authorization".to_string()),
            value_prefix: "Bearer ".to_string(),
            description: "pat".to_string(),
            inject_domains: vec!["api.github.com".to_string()],
            creation_url: None,
            validation: None,
            ambient_sources: Vec::new(),
        })],
    }
}

fn static_binding(manifest: &AuthManifest, entry: Option<CredentialEntry>) -> Arc<AuthBinding> {
    let id = CredentialId::new("github", "pat", "default").unwrap();
    let service = Arc::new(durable_service(
        id.clone(),
        entry,
        Arc::new(TestRefreshSink::default()),
    ));
    let scheme = manifest
        .resolve_static_scheme(Some("pat"))
        .expect("static scheme");
    Arc::new(
        service
            .bind_static(
                id,
                scheme.inject_domains.clone(),
                scheme
                    .header_name
                    .clone()
                    .unwrap_or_else(|| "Authorization".to_owned()),
                scheme.value_prefix.clone(),
            )
            .expect("configured auth"),
    )
}

fn oauth_binding_from_manifest(
    manifest: &AuthManifest,
    entry: Option<CredentialEntry>,
    client_id: Option<String>,
    sink: Arc<dyn RefreshSink>,
) -> Arc<AuthBinding> {
    let id = CredentialId::new("test-provider", "oauth", "default").unwrap();
    let service = Arc::new(durable_service(id.clone(), entry, sink));
    let scheme = manifest
        .resolve_oauth_scheme(Some("oauth"))
        .expect("oauth scheme");
    let request = OAuthRequest::from_runtime(
        scheme.clone(),
        OAuthRuntimeOverrides {
            client_id,
            ..OAuthRuntimeOverrides::default()
        },
    )
    .expect("oauth request");
    Arc::new(
        service
            .bind_oauth(
                id,
                request,
                scheme.inject_domains.clone(),
                scheme
                    .inject_header_name
                    .clone()
                    .unwrap_or_else(|| "Authorization".to_owned()),
                scheme.inject_value_prefix.clone(),
            )
            .expect("configured oauth"),
    )
}

#[tokio::test]
async fn test_missing_credential_fails_closed() {
    let manager = static_binding(&github_pat_manifest(), None);
    let error = manager
        .authorization_for("https://api.github.com/repos")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("no credential is stored"));
}

#[tokio::test]
async fn test_static_token_injection_from_durable_snapshot() {
    let manager = static_binding(
        &github_pat_manifest(),
        Some(CredentialEntry::static_token(SecretString::from(
            "ghp_store_token".to_string(),
        ))),
    );

    assert_eq!(
        manager
            .authorization_for("https://api.github.com/repos")
            .await
            .unwrap(),
        Some((
            "Authorization".to_string(),
            "Bearer ghp_store_token".to_string()
        ))
    );
}

#[tokio::test]
async fn test_auth_manifest_backed_static_token_injection() {
    let manifest = AuthManifest {
        schemes: vec![AuthScheme::StaticToken(StaticTokenScheme {
            key: "pat".to_string(),
            header_name: Some("X-Test-Token".to_string()),
            value_prefix: "token ".to_string(),
            description: "test token".to_string(),
            inject_domains: vec!["api.example.com".to_string()],
            creation_url: None,
            validation: None,
            ambient_sources: Vec::new(),
        })],
    };
    let manager = static_binding(
        &manifest,
        Some(CredentialEntry::static_token(SecretString::from(
            "secret".to_string(),
        ))),
    );

    assert_eq!(
        manager
            .authorization_for("https://api.example.com/repos")
            .await
            .unwrap(),
        Some(("X-Test-Token".to_string(), "token secret".to_string()))
    );
    assert_eq!(
        manager
            .authorization_for("https://other.example.com/repos")
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn test_auth_manifest_backed_static_token_missing_credential_fails_closed() {
    let manifest = AuthManifest {
        schemes: vec![AuthScheme::StaticToken(StaticTokenScheme {
            key: "pat".to_string(),
            header_name: None,
            value_prefix: "Bearer ".to_string(),
            description: "test token".to_string(),
            inject_domains: vec!["api.example.com".to_string()],
            creation_url: None,
            validation: None,
            ambient_sources: Vec::new(),
        })],
    };

    let manager = static_binding(&manifest, None);

    let error = manager
        .authorization_for("https://api.example.com/repos")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("no credential is stored"));
}

#[tokio::test]
async fn refresh_without_stored_oauth_credential_reports_no_credential() {
    let tokens = FakeTokenServer::start(false).await;
    let (auth, _sink, _key) =
        oauth_binding_without_credential(tokens.endpoint(), "localhost".to_string());

    assert_eq!(
        auth.report_rejected_for_response("https://localhost/resource", 401, None,)
            .await,
        RefreshOutcome::NoCredential
    );
    assert_eq!(tokens.refreshes(), 0);
}

#[tokio::test]
async fn test_execute_fetch_returns_denied_when_auth_is_required_but_missing() {
    // Create a mount binding with a config that requires auth for api.github.com
    // but has no stored credential. Authorization must fail closed before the
    // request is dispatched.
    let auth = static_binding(&github_pat_manifest(), None);

    assert!(
        auth.authorization_for("https://api.github.com/repos")
            .await
            .is_err()
    );

    let authority = RuntimeAuthority::for_test(&["api.github.com"], &[], &[]);
    let stack = HttpStack::new(Some(auth), authority).unwrap();

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
    let (auth, sink, _key) = oauth_binding(tokens.endpoint(), FakeHttpsApiServer::domain());
    auth.authorization_for(&api.url())
        .await
        .unwrap()
        .expect("oauth authorization header");

    let stack = HttpStack::with_https_client(
        Some(Arc::clone(&auth)),
        RuntimeAuthority::for_test(&[FakeHttpsApiServer::domain().as_str()], &[], &[]),
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
    assert_eq!(sink.calls.load(Ordering::SeqCst), 1);
    let candidates = sink.candidates.lock().unwrap();
    assert_eq!(
        candidates[0].refreshed.access_token().expose_secret(),
        "access-refresh-1"
    );
}

#[tokio::test]
async fn concurrent_oauth_refreshes_coalesce_inside_one_process() {
    let tokens = FakeTokenServer::start(false).await;
    let (auth, _sink, _key) = oauth_binding(tokens.endpoint(), "localhost".to_string());
    auth.authorization_for("https://localhost/resource")
        .await
        .unwrap()
        .expect("oauth authorization header");

    let results = futures::future::join_all((0..8).map(|_| {
        let auth = Arc::clone(&auth);
        async move {
            auth.report_rejected_for_response("https://localhost/resource", 401, None)
                .await
        }
    }))
    .await;

    assert!(
        results
            .into_iter()
            .all(|result| result == RefreshOutcome::Refreshed)
    );
    assert_eq!(tokens.refreshes(), 1);
}

#[tokio::test]
async fn fetch_blob_uses_same_oauth_retry_path() {
    let tokens = FakeTokenServer::start(false).await;
    let api = FakeHttpsApiServer::start("Bearer access-refresh-1", "blob-body").await;
    let (auth, _sink, _key) = oauth_binding(tokens.endpoint(), FakeHttpsApiServer::domain());
    assert!(auth.applies_to_url(&api.url()));
    assert!(!auth.applies_to_url("https://127.0.0.1/resource"));
    auth.authorization_for(&api.url())
        .await
        .unwrap()
        .expect("oauth authorization header");

    let stack = Arc::new(HttpStack::with_https_client(
        Some(auth),
        RuntimeAuthority::for_test(&[FakeHttpsApiServer::domain().as_str()], &[], &[]),
        test_https_client(),
    ));
    let temp = tempfile::tempdir().unwrap();
    let caches = Caches::open(temp.path()).unwrap();
    let name = omnifs_core::ResourceName::new("test").unwrap();
    let resources = mount(
        &caches,
        &name,
        b"auth-test",
        omnifs_core::ProviderId::from_wasm_bytes(b"auth-test-provider"),
    )
    .unwrap();
    let executor = BlobExecutor::new(stack, resources, BlobLimits::default());

    let result = executor
        .fetch(
            &wit_types::BlobFetchRequest {
                method: "GET".to_string(),
                url: api.url(),
                headers: Vec::new(),
                body: None,
            },
            0,
        )
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
async fn oauth_refresh_failure_surfaces_denied_and_preserves_snapshot() {
    let tokens = FakeTokenServer::start(true).await;
    let api = FakeHttpsApiServer::start("Bearer never-used", "ok").await;
    let (auth, sink, _key) = oauth_binding(tokens.endpoint(), FakeHttpsApiServer::domain());
    auth.authorization_for(&api.url())
        .await
        .unwrap()
        .expect("oauth authorization header");

    let stack = HttpStack::with_https_client(
        Some(Arc::clone(&auth)),
        RuntimeAuthority::for_test(&[FakeHttpsApiServer::domain().as_str()], &[], &[]),
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
    assert_eq!(sink.calls.load(Ordering::SeqCst), 0);
    let err = auth.authorization_for(&api.url()).await.unwrap_err();
    assert!(err.to_string().contains("needs re-authentication"));
}

#[tokio::test]
async fn oauth_config_client_id_overrides_missing_manifest_default_for_refresh() {
    let tokens = FakeTokenServer::start_with_expected_client_id(false, Some("byo-client")).await;

    let mut manifest = oauth_manifest(tokens.endpoint(), "localhost".to_string());
    let AuthScheme::Oauth(scheme) = &mut manifest.schemes[0] else {
        panic!("expected oauth scheme");
    };
    scheme.default_client_id = None;

    let auth = oauth_binding_from_manifest(
        &manifest,
        Some(oauth_entry(
            "old-access",
            "refresh-1",
            OffsetDateTime::now_utc() + time::Duration::seconds(3600),
        )),
        Some("byo-client".to_owned()),
        Arc::new(TestRefreshSink::default()),
    );
    assert_eq!(
        auth.report_rejected_for_response("https://localhost/resource", 401, None,)
            .await,
        RefreshOutcome::Refreshed
    );
    assert_eq!(tokens.refreshes(), 1);
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

fn oauth_binding(
    token_endpoint: String,
    inject_domain: String,
) -> (Arc<AuthBinding>, Arc<TestRefreshSink>, CredentialId) {
    let key = CredentialId::new("test-provider", "oauth", "default").unwrap();
    let sink = Arc::new(TestRefreshSink::default());
    let auth = oauth_binding_from_manifest(
        &oauth_manifest(token_endpoint, inject_domain),
        Some(oauth_entry(
            "old-access",
            "refresh-1",
            OffsetDateTime::now_utc() + time::Duration::seconds(3600),
        )),
        None,
        Arc::clone(&sink) as Arc<dyn RefreshSink>,
    );
    (auth, sink, key)
}

fn oauth_binding_without_credential(
    token_endpoint: String,
    inject_domain: String,
) -> (Arc<AuthBinding>, Arc<TestRefreshSink>, CredentialId) {
    let key = CredentialId::new("test-provider", "oauth", "default").unwrap();
    let sink = Arc::new(TestRefreshSink::default());
    let auth = oauth_binding_from_manifest(
        &oauth_manifest(token_endpoint, inject_domain),
        None,
        None,
        Arc::clone(&sink) as Arc<dyn RefreshSink>,
    );
    (auth, sink, key)
}

fn durable_service(
    id: CredentialId,
    entry: Option<CredentialEntry>,
    sink: Arc<dyn RefreshSink>,
) -> CredentialService {
    let snapshots = entry.map(|entry| {
        (
            id,
            DurableCredentialSnapshot {
                entry,
                version: CredentialVersion::initial(),
            },
        )
    });
    CredentialService::new(snapshots, OAuthClient::new().expect("oauth client"), sink)
}

struct TestRefreshSink {
    calls: AtomicUsize,
    candidates: std::sync::Mutex<Vec<RefreshCandidate>>,
}

impl Default for TestRefreshSink {
    fn default() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            candidates: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl RefreshSink for TestRefreshSink {
    fn persist<'a>(
        &'a self,
        candidate: RefreshCandidate,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<RefreshPersistence, RefreshPersistError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let version = candidate.expected_version.next().expect("version advances");
            self.candidates.lock().unwrap().push(candidate);
            Ok(RefreshPersistence::Active { version })
        })
    }
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
