use crate::callback::LoopbackCallback;
use crate::client::BoxFuture;
use crate::{AuthError, ManualCode, UrlOpener};
use omnifs_workspace::authn::{
    ClientSideTokenConfig, DeviceCodeConfig, DevicePollCompat, KeyValue, OAuthFlow, OauthScheme,
    PkceLoopbackConfig, PkceManualCodeConfig, TokenEndpointAuthMethod,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use url::Url;

#[derive(Clone, Default)]
pub(super) struct FakeBehavior {
    pub(super) state_override: Option<String>,
    pub(super) token_error: Option<(String, String)>,
    pub(super) device_pending_responses: usize,
    /// Simulates a non-RFC-8628 token endpoint: the pending response comes
    /// back as `200 OK` (with the same error body) instead of `400`.
    pub(super) device_pending_ok_body: bool,
    pub(super) refresh_delay_ms: u64,
}

pub(super) struct FakeOpener(pub(super) FakeAuthServer);

impl UrlOpener for FakeOpener {
    fn open<'a>(&'a self, url: &'a Url) -> BoxFuture<'a, Result<(), AuthError>> {
        Box::pin(async move {
            let fake = self.0.clone();
            let url = url.clone();
            tokio::spawn(async move {
                fake.follow_authorize_redirect(url).await;
            });
            Ok(())
        })
    }
}

#[derive(Clone)]
pub(super) struct FakeAuthServer {
    base: Url,
    state: Arc<FakeState>,
}

struct FakeState {
    behavior: FakeBehavior,
    codes: Mutex<HashMap<String, String>>,
    device_pending_remaining: AtomicUsize,
    next_token: AtomicUsize,
    refreshes: AtomicUsize,
}

impl FakeAuthServer {
    pub(super) async fn start(behavior: FakeBehavior) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let device_pending_responses = behavior.device_pending_responses;
        let server = Self {
            base: Url::parse(&format!("http://{addr}")).unwrap(),
            state: Arc::new(FakeState {
                behavior,
                codes: Mutex::new(HashMap::new()),
                device_pending_remaining: AtomicUsize::new(device_pending_responses),
                next_token: AtomicUsize::new(1),
                refreshes: AtomicUsize::new(0),
            }),
        };
        let task_server = server.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let task_server = task_server.clone();
                tokio::spawn(async move {
                    task_server.handle(stream).await;
                });
            }
        });
        server
    }

    pub(super) fn loopback_scheme(&self, revocation_endpoint: Option<String>) -> OauthScheme {
        self.scheme(
            OAuthFlow::PkceLoopback(PkceLoopbackConfig {
                redirect_uri_template: "http://127.0.0.1:{port}/callback".to_owned(),
            }),
            revocation_endpoint,
        )
    }

    pub(super) fn manual_scheme(&self, revocation_endpoint: Option<String>) -> OauthScheme {
        self.scheme(
            OAuthFlow::PkceManualCode(PkceManualCodeConfig {
                redirect_uri: "http://127.0.0.1/manual".to_owned(),
            }),
            revocation_endpoint,
        )
    }

    pub(super) fn client_side_scheme(&self, revocation_endpoint: Option<String>) -> OauthScheme {
        self.scheme(
            OAuthFlow::ClientSideToken(ClientSideTokenConfig {
                redirect_uri_template: "http://127.0.0.1:{port}/callback".to_owned(),
            }),
            revocation_endpoint,
        )
    }

    pub(super) fn device_scheme(
        &self,
        device_poll_compat: DevicePollCompat,
        revocation_endpoint: Option<String>,
    ) -> OauthScheme {
        self.scheme(
            OAuthFlow::DeviceCode(DeviceCodeConfig {
                device_authorization_endpoint: self.endpoint("/device"),
                device_poll_compat,
            }),
            revocation_endpoint,
        )
    }

    fn scheme(&self, flow: OAuthFlow, revocation_endpoint: Option<String>) -> OauthScheme {
        OauthScheme {
            key: "oauth".to_owned(),
            display_name: "fake".to_owned(),
            authorization_endpoint: self.endpoint("/authorize"),
            token_endpoint: self.endpoint("/token"),
            revocation_endpoint,
            default_client_id: Some("client-id".to_owned()),
            default_scopes: vec!["read".to_owned(), "write".to_owned()],
            flow,
            token_endpoint_auth: TokenEndpointAuthMethod::None,
            refresh_token_rotates: true,
            extra_authorize_params: vec![KeyValue {
                key: "audience".to_owned(),
                value: "omnifs-test".to_owned(),
            }],
            extra_token_params: vec![KeyValue {
                key: "resource".to_owned(),
                value: "omnifs-test".to_owned(),
            }],
            inject_domains: vec!["api.example.test".to_owned()],
            inject_header_name: None,
            inject_value_prefix: "Bearer ".to_owned(),
        }
    }

    fn endpoint(&self, path: &str) -> String {
        self.base.join(path).unwrap().to_string()
    }

    async fn follow_authorize_redirect(&self, url: Url) {
        let client = reqwest::ClientBuilder::new()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let response = client.get(url).send().await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::FOUND);
        let redirect = response
            .headers()
            .get(reqwest::header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        let redirect = Url::parse(redirect).unwrap();
        let fragment = redirect.fragment().map(str::to_owned);
        let mut first_callback = redirect.clone();
        first_callback.set_fragment(None);
        let response = client.get(first_callback.clone()).send().await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        if let Some(fragment) = fragment {
            let mut second_callback = first_callback;
            second_callback.set_query(Some(&fragment));
            let response = client.get(second_callback).send().await.unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::OK);
        }
    }

    pub(super) async fn manual_authorize(&self, url: Url) -> Result<ManualCode, AuthError> {
        let client = reqwest::ClientBuilder::new()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let response = client.get(url).send().await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::FOUND);
        let redirect = response
            .headers()
            .get(reqwest::header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        let redirect = Url::parse(redirect).unwrap();
        let parsed = LoopbackCallback::parse(&redirect)?;
        Ok(ManualCode {
            code: parsed.code,
            state: parsed.state,
        })
    }

    pub(super) fn refreshes(&self) -> usize {
        self.state.refreshes.load(Ordering::SeqCst)
    }

    async fn handle(&self, mut stream: tokio::net::TcpStream) {
        let mut buf = vec![0; 8192];
        let read = stream.read(&mut buf).await.unwrap();
        let request = String::from_utf8_lossy(&buf[..read]);
        let mut parts = request.split("\r\n\r\n");
        let head = parts.next().unwrap_or_default();
        let body = parts.next().unwrap_or_default();
        let request_line = head.lines().next().unwrap_or_default();
        let mut request_parts = request_line.split_ascii_whitespace();
        let method = request_parts.next().unwrap_or_default();
        let target = request_parts.next().unwrap_or_default();

        match (method, target.split('?').next().unwrap_or_default()) {
            ("GET", "/authorize") => self.handle_authorize(&mut stream, target).await,
            ("POST", "/device") => self.handle_device(&mut stream, body).await,
            ("POST", "/token") => self.handle_token(&mut stream, body).await,
            _ => {
                write_fake_response(&mut stream, "404 Not Found", "text/plain", "not found").await;
            },
        }
    }

    async fn handle_authorize(&self, stream: &mut tokio::net::TcpStream, target: &str) {
        let url = Url::parse(&format!("http://localhost{target}")).unwrap();
        let params: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(
            params.get("client_id").map(String::as_str),
            Some("client-id")
        );
        assert_eq!(
            params.get("audience").map(String::as_str),
            Some("omnifs-test")
        );
        let redirect_uri = params.get("redirect_uri").unwrap();
        let state = params.get("state").unwrap();
        let returned_state = self.state.behavior.state_override.as_ref().unwrap_or(state);
        let redirect = match params.get("response_type").map(String::as_str) {
            Some("code") => {
                assert_eq!(
                    params.get("code_challenge_method").map(String::as_str),
                    Some("S256")
                );
                let code = format!("code-{}", self.state.next_token.load(Ordering::SeqCst));
                self.state
                    .codes
                    .lock()
                    .await
                    .insert(code.clone(), params.get("code_challenge").unwrap().clone());
                Url::parse_with_params(
                    redirect_uri,
                    &[("code", code.as_str()), ("state", returned_state.as_str())],
                )
                .unwrap()
            },
            Some("token") => {
                let mut redirect = Url::parse(redirect_uri).unwrap();
                let id = self.state.next_token.fetch_add(1, Ordering::SeqCst);
                let fragment = url::form_urlencoded::Serializer::new(String::new())
                    .append_pair("access_token", &format!("implicit-access-{id}"))
                    .append_pair("token_type", "bearer")
                    .append_pair("expires_in", "2592000")
                    .append_pair("scope", "read write")
                    .append_pair("state", returned_state)
                    .finish();
                redirect.set_fragment(Some(&fragment));
                redirect
            },
            other => panic!("unexpected response_type: {other:?}"),
        };
        let response = format!(
            "HTTP/1.1 302 Found\r\nlocation: {redirect}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    }

    async fn handle_device(&self, stream: &mut tokio::net::TcpStream, body: &str) {
        let params: HashMap<String, String> = url::form_urlencoded::parse(body.as_bytes())
            .into_owned()
            .collect();
        assert_eq!(
            params.get("client_id").map(String::as_str),
            Some("client-id")
        );
        assert_eq!(params.get("scope").map(String::as_str), Some("read write"));
        assert_eq!(
            params.get("audience").map(String::as_str),
            Some("omnifs-test")
        );
        let body = serde_json::json!({
            "device_code": "device-1",
            "user_code": "WDJB-MJHT",
            "verification_uri": "https://example.test/device",
            "verification_uri_complete": "https://example.test/device?user_code=WDJB-MJHT",
            "expires_in": 900,
            "interval": 0,
        })
        .to_string();
        write_fake_response(stream, "200 OK", "application/json", &body).await;
    }

    #[allow(clippy::too_many_lines)]
    async fn handle_token(&self, stream: &mut tokio::net::TcpStream, body: &str) {
        let params: HashMap<String, String> = url::form_urlencoded::parse(body.as_bytes())
            .into_owned()
            .collect();
        assert_eq!(
            params.get("client_id").map(String::as_str),
            Some("client-id")
        );
        assert_eq!(
            params.get("resource").map(String::as_str),
            Some("omnifs-test")
        );

        if let Some((error, description)) = &self.state.behavior.token_error {
            let body = serde_json::json!({
                "error": error,
                "error_description": description,
            })
            .to_string();
            write_fake_response(stream, "400 Bad Request", "application/json", &body).await;
            return;
        }

        match params.get("grant_type").map(String::as_str) {
            Some("authorization_code") => {
                assert!(params.contains_key("code_verifier"));
                assert!(
                    self.state
                        .codes
                        .lock()
                        .await
                        .remove(params.get("code").unwrap())
                        .is_some()
                );
                let id = self.state.next_token.fetch_add(1, Ordering::SeqCst);
                let body = serde_json::json!({
                    "access_token": format!("access-{id}"),
                    "refresh_token": format!("refresh-{id}"),
                    "expires_in": 3600,
                    "token_type": "Bearer",
                    "scope": "read write",
                })
                .to_string();
                write_fake_response(stream, "200 OK", "application/json", &body).await;
            },
            Some("urn:ietf:params:oauth:grant-type:device_code") => {
                assert_eq!(
                    params.get("device_code").map(String::as_str),
                    Some("device-1")
                );
                if self
                    .state
                    .device_pending_remaining
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                        remaining.checked_sub(1)
                    })
                    .is_ok()
                {
                    let body = serde_json::json!({
                        "error": "authorization_pending",
                        "error_description": "The authorization request is still pending.",
                    })
                    .to_string();
                    let status = if self.state.behavior.device_pending_ok_body {
                        "200 OK"
                    } else {
                        "400 Bad Request"
                    };
                    write_fake_response(stream, status, "application/json", &body).await;
                    return;
                }
                let id = self.state.next_token.fetch_add(1, Ordering::SeqCst);
                let body = serde_json::json!({
                    "access_token": format!("device-access-{id}"),
                    "token_type": "Bearer",
                    "scope": "read write",
                })
                .to_string();
                write_fake_response(stream, "200 OK", "application/json", &body).await;
            },
            Some("refresh_token") => {
                assert_eq!(
                    params.get("refresh_token").map(String::as_str),
                    Some("refresh-1")
                );
                if self.state.behavior.refresh_delay_ms > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(
                        self.state.behavior.refresh_delay_ms,
                    ))
                    .await;
                }
                let id = self.state.refreshes.fetch_add(1, Ordering::SeqCst) + 1;
                let body = serde_json::json!({
                    "access_token": format!("access-refresh-{id}"),
                    "refresh_token": format!("refresh-rotated-{id}"),
                    "expires_in": 3600,
                    "token_type": "Bearer",
                    "scope": "read write",
                })
                .to_string();
                write_fake_response(stream, "200 OK", "application/json", &body).await;
            },
            other => panic!("unexpected grant_type: {other:?}"),
        }
    }
}

#[derive(Clone)]
pub(super) struct FakeRevocationServer {
    base: Url,
    revocations: Arc<AtomicUsize>,
    fail: bool,
}

impl FakeRevocationServer {
    pub(super) async fn start() -> Self {
        Self::start_with_failure(false).await
    }

    pub(super) async fn start_with_failure(fail: bool) -> Self {
        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
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
            base: Url::parse(&format!("https://{addr}")).unwrap(),
            revocations: Arc::new(AtomicUsize::new(0)),
            fail,
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
                    let mut buf = vec![0; 4096];
                    let Ok(read) = stream.read(&mut buf).await else {
                        return;
                    };
                    let request = String::from_utf8_lossy(&buf[..read]);
                    if !request.starts_with("POST /revoke ") {
                        return;
                    }
                    task_server.revocations.fetch_add(1, Ordering::SeqCst);
                    if task_server.fail {
                        let response = "HTTP/1.1 500 Internal Server Error\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}";
                        stream.write_all(response.as_bytes()).await.unwrap();
                        return;
                    }
                    let response = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}";
                    stream.write_all(response.as_bytes()).await.unwrap();
                });
            }
        });
        server
    }

    pub(super) fn endpoint(&self) -> String {
        self.base.join("/revoke").unwrap().to_string()
    }

    pub(super) fn revocations(&self) -> usize {
        self.revocations.load(Ordering::SeqCst)
    }
}

async fn write_fake_response(
    stream: &mut tokio::net::TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
) {
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await.unwrap();
}
