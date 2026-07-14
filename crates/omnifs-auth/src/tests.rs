use crate::callback::{LoopbackCallback, LoopbackEndpoint, accept_callback_request};
use crate::request::ClientSideTokenLoginRequest;
use crate::test_support::{FakeAuthServer, FakeBehavior, FakeOpener};
use crate::{
    AuthBinding, AuthError, CredentialService, LoginRequest, OAuthClient, OAuthRequest,
    RefreshOutcome, UrlOpener,
};
use omnifs_workspace::authn::{
    CredentialId, DevicePollCompat, OAuthFlow, OauthScheme, PkceManualCodeConfig,
    TokenEndpointAuthMethod,
};
use omnifs_workspace::creds::{
    CredentialEntry, CredentialStore, FileStore, MemoryStore, Refreshability,
};
use omnifs_workspace::mounts::OAuth as OAuthMount;
use secrecy::{ExposeSecret, SecretString};
use std::sync::Arc;
use time::OffsetDateTime;
use url::Url;

#[tokio::test]
async fn pkce_loopback_login_against_fake_server() {
    let fake = FakeAuthServer::start(FakeBehavior::default()).await;
    let scheme = fake.loopback_scheme(None);
    let opener: Arc<dyn UrlOpener> = Arc::new(FakeOpener(fake.clone()));
    let client = OAuthClient::new().unwrap().with_opener(opener);

    let entry = client
        .login_loopback(loopback_login_request(scheme))
        .await
        .unwrap();

    assert_eq!(entry.access_token().expose_secret(), "access-1");
    assert_eq!(entry.refresh_token().unwrap().expose_secret(), "refresh-1");
    assert_eq!(entry.token_type(), "bearer");
    assert_eq!(entry.scopes(), ["read", "write"]);
}

#[tokio::test]
async fn pkce_manual_code_login_against_fake_server() {
    let fake = FakeAuthServer::start(FakeBehavior::default()).await;
    let scheme = fake.manual_scheme(None);
    let client = OAuthClient::new().unwrap();
    let entry = client
        .login_manual_code(manual_code_login_request(scheme), |url| {
            let fake = fake.clone();
            async move { fake.manual_authorize(url).await }
        })
        .await
        .unwrap();

    assert_eq!(entry.access_token().expose_secret(), "access-1");
    assert_eq!(entry.refresh_token().unwrap().expose_secret(), "refresh-1");
}

#[tokio::test]
async fn client_side_token_login_captures_fragment_token() {
    let fake = FakeAuthServer::start(FakeBehavior::default()).await;
    let scheme = fake.client_side_scheme(None);
    let opener: Arc<dyn UrlOpener> = Arc::new(FakeOpener(fake.clone()));
    let client = OAuthClient::new().unwrap().with_opener(opener);

    let entry = client
        .login_client_side_token(client_side_token_login_request(scheme))
        .await
        .unwrap();

    assert_eq!(entry.access_token().expose_secret(), "implicit-access-1");
    assert!(entry.refresh_token().is_none());
    assert_eq!(entry.refreshability(), Refreshability::NotRefreshable);
    assert_eq!(entry.token_type(), "bearer");
    assert_eq!(entry.scopes(), ["read", "write"]);
    assert!(entry.expires_at().is_some());
}

#[tokio::test]
async fn device_code_login_against_fake_server() {
    let fake = FakeAuthServer::start(FakeBehavior::default()).await;
    let scheme = fake.device_scheme(DevicePollCompat::Rfc8628, None);
    let client = OAuthClient::new().unwrap();

    let entry = client
        .login_device_code(device_code_login_request(scheme), |prompt| async move {
            assert_eq!(prompt.verification_uri, "https://example.test/device");
            assert_eq!(
                prompt.verification_uri_complete.as_deref(),
                Some("https://example.test/device?user_code=WDJB-MJHT")
            );
            assert_eq!(prompt.user_code, "WDJB-MJHT");
            Ok(())
        })
        .await
        .unwrap();

    assert_eq!(entry.access_token().expose_secret(), "device-access-1");
    assert!(entry.refresh_token().is_none());
    assert_eq!(entry.scopes(), ["read", "write"]);
}

#[tokio::test]
async fn device_code_login_polls_past_pending_response() {
    let fake = FakeAuthServer::start(FakeBehavior {
        device_pending_responses: 1,
        ..FakeBehavior::default()
    })
    .await;
    let scheme = fake.device_scheme(DevicePollCompat::Rfc8628, None);
    let client = OAuthClient::new().unwrap();

    let entry = client
        .login_device_code(device_code_login_request(scheme), |_| async { Ok(()) })
        .await
        .unwrap();

    assert_eq!(entry.access_token().expose_secret(), "device-access-1");
}

/// A non-RFC-8628 token endpoint returns `200 OK` with an error body while
/// pending. A scheme that declares `DevicePollCompat::ErrorInOkBody` applies
/// the host rewrite, so the poll loop treats it as a continue signal and the
/// login still succeeds.
#[tokio::test]
async fn device_code_login_rewrites_pending_ok_body_when_declared() {
    let fake = FakeAuthServer::start(FakeBehavior {
        device_pending_responses: 1,
        device_pending_ok_body: true,
        ..FakeBehavior::default()
    })
    .await;
    let scheme = fake.device_scheme(DevicePollCompat::ErrorInOkBody, None);
    let client = OAuthClient::new().unwrap();

    let entry = client
        .login_device_code(device_code_login_request(scheme), |_| async { Ok(()) })
        .await
        .unwrap();

    assert_eq!(entry.access_token().expose_secret(), "device-access-1");
}

/// Without declaring `DevicePollCompat::ErrorInOkBody`, the rewrite shim is a
/// no-op: a `200 OK` pending response is parsed as a (malformed) success
/// response and the login fails on the first poll instead of continuing.
#[tokio::test]
async fn device_code_login_rfc8628_does_not_rewrite_pending_ok_body() {
    let fake = FakeAuthServer::start(FakeBehavior {
        device_pending_responses: 1,
        device_pending_ok_body: true,
        ..FakeBehavior::default()
    })
    .await;
    let scheme = fake.device_scheme(DevicePollCompat::Rfc8628, None);
    let client = OAuthClient::new().unwrap();

    let result = client
        .login_device_code(device_code_login_request(scheme), |_| async { Ok(()) })
        .await;

    assert!(
        result.is_err(),
        "expected the unrewritten OK body to fail parsing"
    );
}

#[tokio::test]
async fn loopback_endpoint_exposes_concrete_redirect_uri() {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let fixed_port = probe.local_addr().unwrap().port();
    drop(probe);

    let fixed_template = format!("http://127.0.0.1:{fixed_port}/callback");
    let fixed = LoopbackEndpoint::bind(&fixed_template).await.unwrap();
    assert_eq!(fixed.redirect_uri().as_str(), fixed_template);

    let dynamic = LoopbackEndpoint::bind("http://127.0.0.1:{port}/callback")
        .await
        .unwrap();
    let dynamic_url = Url::parse(dynamic.redirect_uri().as_str()).unwrap();
    assert_eq!(dynamic_url.host_str(), Some("127.0.0.1"));
    assert!(dynamic_url.port().is_some_and(|port| port > 0));

    assert!(matches!(
        LoopbackEndpoint::bind("https://example.com/callback").await,
        Err(AuthError::InvalidRedirectUri)
    ));
}

#[test]
fn loopback_callback_surfaces_authorization_error() {
    let err = LoopbackCallback::parse(
        &Url::parse("http://127.0.0.1/callback?error=access_denied&error_description=denied")
            .unwrap(),
    )
    .unwrap_err();

    assert!(matches!(
        err,
        AuthError::AuthorizationError {
            error,
            description
        } if error == "access_denied" && description.as_deref() == Some("denied")
    ));
}

#[test]
fn loopback_callback_requires_code_and_state() {
    let missing_code =
        LoopbackCallback::parse(&Url::parse("http://127.0.0.1/callback?state=ok").unwrap())
            .unwrap_err();
    assert!(matches!(missing_code, AuthError::MissingCode));

    let missing_state =
        LoopbackCallback::parse(&Url::parse("http://127.0.0.1/callback?code=ok").unwrap())
            .unwrap_err();
    assert!(matches!(missing_state, AuthError::MissingState));
}

/// The loopback callback listener accepts only GET (a browser redirect never
/// issues anything else): a POST is answered 405 and surfaces as
/// `InvalidCallback`, so a stray non-GET request never completes the flow.
#[tokio::test]
async fn loopback_callback_rejects_non_get_method() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let client = tokio::spawn(async move {
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"POST /callback?code=c&state=s HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        String::from_utf8_lossy(&buf).into_owned()
    });

    let result = accept_callback_request(&listener).await;
    assert!(
        matches!(result, Err(AuthError::InvalidCallback)),
        "a non-GET callback is rejected as InvalidCallback"
    );

    let response = client.await.unwrap();
    assert!(
        response.starts_with("HTTP/1.1 405"),
        "the client receives 405 Method Not Allowed, got: {response}"
    );
}

#[tokio::test]
async fn csrf_state_mismatch_is_rejected() {
    let fake = FakeAuthServer::start(FakeBehavior {
        state_override: Some("wrong-state".to_owned()),
        ..FakeBehavior::default()
    })
    .await;
    let scheme = fake.loopback_scheme(None);
    let opener: Arc<dyn UrlOpener> = Arc::new(FakeOpener(fake.clone()));
    let client = OAuthClient::new().unwrap().with_opener(opener);

    let err = client
        .login_loopback(loopback_login_request(scheme))
        .await
        .unwrap_err();

    assert!(matches!(err, AuthError::StateMismatch));
}

#[tokio::test]
async fn token_endpoint_errors_surface_typed_errors() {
    let fake = FakeAuthServer::start(FakeBehavior {
        token_error: Some(("invalid_grant".to_owned(), "bad code".to_owned())),
        ..FakeBehavior::default()
    })
    .await;
    let scheme = fake.manual_scheme(None);
    let client = OAuthClient::new().unwrap();

    let err = client
        .login_manual_code(manual_code_login_request(scheme), |url| {
            let fake = fake.clone();
            async move { fake.manual_authorize(url).await }
        })
        .await
        .unwrap_err();

    assert!(matches!(
        err,
        AuthError::TokenEndpoint {
            error,
            description
        } if error == "invalid_grant" && description.as_deref() == Some("bad code")
    ));
}

#[tokio::test]
async fn refresh_exchange_parses_rotated_refresh_token() {
    let fake = FakeAuthServer::start(FakeBehavior::default()).await;
    let scheme = fake.loopback_scheme(None);
    let client = OAuthClient::new().unwrap();

    let entry = client
        .refresh(
            OAuthRequest::new(scheme),
            SecretString::from("refresh-1".to_owned()),
        )
        .await
        .unwrap();

    assert_eq!(entry.access_token().expose_secret(), "access-refresh-1");
    assert_eq!(
        entry.refresh_token().unwrap().expose_secret(),
        "refresh-rotated-1"
    );
}

#[tokio::test]
async fn device_code_refresh_does_not_require_redirect_uri() {
    let fake = FakeAuthServer::start(FakeBehavior::default()).await;
    let scheme = fake.device_scheme(DevicePollCompat::Rfc8628, None);
    let client = OAuthClient::new().unwrap();

    let entry = client
        .refresh(
            OAuthRequest::new(scheme),
            SecretString::from("refresh-1".to_owned()),
        )
        .await
        .unwrap();

    assert_eq!(entry.access_token().expose_secret(), "access-refresh-1");
}

#[tokio::test]
async fn binding_rejection_single_flights_refresh() {
    let fake = FakeAuthServer::start(FakeBehavior {
        refresh_delay_ms: 50,
        ..FakeBehavior::default()
    })
    .await;
    let (binding, store, id) = binding_with_oauth(fake.loopback_scheme(None));
    seed_oauth(store.as_ref(), &id, "old-access", "refresh-1", 3600);
    binding
        .authorization_for("https://api.example.test/resource")
        .await
        .unwrap();

    // Collected eagerly so all tasks are spawned (and observe the pre-rotation
    // token) before any is awaited; a lazy iterator would serialize them.
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let binding = Arc::clone(&binding);
            tokio::spawn(async move {
                binding
                    .report_rejected_for_response("https://api.example.test/resource", 401, None)
                    .await
            })
        })
        .collect();
    let mut results = Vec::new();
    for handle in handles {
        results.push(handle.await.unwrap());
    }

    assert!(
        results
            .iter()
            .all(|result| *result == RefreshOutcome::Refreshed)
    );
    assert_eq!(fake.refreshes(), 1);
    assert_eq!(
        store
            .get(&id)
            .unwrap()
            .unwrap()
            .access_token()
            .expose_secret(),
        "access-refresh-1"
    );
    assert!(matches!(binding.health(), crate::CredentialHealth::Ready));
}

#[tokio::test]
async fn report_rejected_403_bearer_invalid_token_refreshes() {
    let fake = FakeAuthServer::start(FakeBehavior::default()).await;
    let (binding, store, id) = binding_with_oauth(fake.loopback_scheme(None));
    seed_oauth(store.as_ref(), &id, "old-access", "refresh-1", 3600);
    binding
        .authorization_for("https://api.example.test/resource")
        .await
        .unwrap();

    let outcome = binding
        .report_rejected_for_response(
            "https://api.example.test/resource",
            403,
            Some(r#"Bearer realm="api", error="invalid_token""#.to_owned()),
        )
        .await;

    assert_eq!(outcome, RefreshOutcome::Refreshed);
    assert_eq!(fake.refreshes(), 1);
}

#[tokio::test]
async fn report_rejected_403_unrelated_challenge_does_not_refresh() {
    let fake = FakeAuthServer::start(FakeBehavior::default()).await;
    let (binding, store, id) = binding_with_oauth(fake.loopback_scheme(None));
    seed_oauth(store.as_ref(), &id, "old-access", "refresh-1", 3600);
    binding
        .authorization_for("https://api.example.test/resource")
        .await
        .unwrap();

    let outcome = binding
        .report_rejected_for_response(
            "https://api.example.test/resource",
            403,
            Some(r#"Bearer error="not_invalid_token""#.to_owned()),
        )
        .await;

    assert_eq!(outcome, RefreshOutcome::NotApplicable);
    assert_eq!(fake.refreshes(), 0);
}

#[test]
fn bind_reuses_identical_oauth_runtime_metadata_but_rejects_conflicts() {
    let service = Arc::new(CredentialService::new(
        Arc::new(MemoryStore::default()),
        OAuthClient::new().unwrap(),
    ));
    let id = CredentialId::new("test-provider", "oauth", "default").unwrap();

    let mut first = binding_scheme();
    first.inject_domains = vec!["first.example.test".to_owned()];
    first.inject_header_name = Some("X-First".to_owned());
    first.inject_value_prefix = "Token ".to_owned();
    let first = service
        .bind_oauth(
            id.clone(),
            OAuthRequest::new(first),
            vec!["first.example.test".to_owned()],
            "X-First".to_owned(),
            "Token ".to_owned(),
        )
        .unwrap();

    let mut same_runtime = binding_scheme();
    same_runtime.inject_domains = vec!["second.example.test".to_owned()];
    same_runtime.inject_header_name = Some("X-Second".to_owned());
    same_runtime.inject_value_prefix = "Bearer ".to_owned();
    let second = service
        .bind_oauth(
            id.clone(),
            OAuthRequest::new(same_runtime),
            vec!["second.example.test".to_owned()],
            "X-Second".to_owned(),
            "Bearer ".to_owned(),
        )
        .unwrap();

    let conflicting = binding_scheme();
    let conflicting_request = OAuthRequest::from_mount_config(
        Some(&OAuthMount {
            client_id: Some("different-client".to_owned()),
            ..OAuthMount::default()
        }),
        conflicting,
    )
    .unwrap();
    let third = service
        .bind_oauth(
            id.clone(),
            conflicting_request,
            vec![],
            "Authorization".to_owned(),
            "Bearer ".to_owned(),
        )
        .unwrap();
    assert!(first.same_runtime_as(&second));
    assert!(!first.same_runtime_as(&third));
}

#[tokio::test]
async fn shared_binding_adopts_fresh_stored_token_without_second_refresh() {
    let fake = FakeAuthServer::start(FakeBehavior::default()).await;
    let store: Arc<dyn CredentialStore> = Arc::new(MemoryStore::default());
    let id = CredentialId::new("test-provider", "oauth", "default").unwrap();
    seed_oauth(store.as_ref(), &id, "old-access", "refresh-1", 3600);
    let http = reqwest::ClientBuilder::new()
        .danger_accept_invalid_certs(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let service = Arc::new(CredentialService::new(
        Arc::clone(&store),
        OAuthClient::from_http_client(http),
    ));
    let request = OAuthRequest::new(fake.loopback_scheme(None));
    let first = Arc::new(
        service
            .bind_oauth(
                id.clone(),
                request.clone(),
                vec!["api.example.test".to_owned()],
                "Authorization".to_owned(),
                "Bearer ".to_owned(),
            )
            .unwrap(),
    );
    let follower = Arc::new(
        service
            .bind_oauth(
                id.clone(),
                request,
                vec!["api.example.test".to_owned()],
                "Authorization".to_owned(),
                "Bearer ".to_owned(),
            )
            .unwrap(),
    );

    first
        .authorization_for("https://api.example.test/resource")
        .await
        .unwrap();
    assert_eq!(
        first
            .report_rejected_for_response("https://api.example.test/resource", 401, None)
            .await,
        RefreshOutcome::Refreshed
    );
    assert_eq!(fake.refreshes(), 1);
    assert_eq!(
        follower
            .report_rejected_for_response("https://api.example.test/resource", 401, None)
            .await,
        RefreshOutcome::Refreshed
    );
    assert_eq!(fake.refreshes(), 1);
    assert_eq!(
        follower
            .authorization_for("https://api.example.test/resource")
            .await
            .unwrap()
            .unwrap()
            .1,
        "Bearer access-refresh-1"
    );
}

#[test]
fn bind_rejects_stored_kind_mismatch() {
    let store: Arc<dyn CredentialStore> = Arc::new(MemoryStore::default());
    let id = CredentialId::new("test-provider", "pat", "default").unwrap();
    store
        .put(
            &id,
            &CredentialEntry::oauth(
                SecretString::from("access"),
                None,
                None,
                "Bearer",
                vec![],
                OffsetDateTime::UNIX_EPOCH,
            ),
        )
        .unwrap();
    let service = Arc::new(CredentialService::new(store, OAuthClient::new().unwrap()));

    assert!(matches!(
        service.bind_static(id.clone(), vec![], "Authorization".to_owned(), "Bearer ".to_owned()),
        Err(AuthError::CredentialKindMismatch { id: error_id, .. }) if error_id == id
    ));
}

#[test]
fn bind_propagates_malformed_store_reads() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("credentials.json");
    std::fs::write(&path, b"not json").unwrap();
    let service = Arc::new(CredentialService::new(
        Arc::new(FileStore::new(path)),
        OAuthClient::new().unwrap(),
    ));
    let id = CredentialId::new("test-provider", "pat", "default").unwrap();

    assert!(matches!(
        service.bind_static(id, vec![], "Authorization".to_owned(), "Bearer ".to_owned()),
        Err(AuthError::CredentialStore(_))
    ));
}

#[tokio::test]
async fn invalid_grant_refresh_needs_consent_and_keeps_stored_entry() {
    let fake = FakeAuthServer::start(FakeBehavior {
        token_error: Some(("invalid_grant".to_owned(), "revoked".to_owned())),
        ..FakeBehavior::default()
    })
    .await;
    let (binding, store, id) = binding_with_oauth(fake.loopback_scheme(None));
    seed_oauth(store.as_ref(), &id, "old-access", "refresh-1", 3600);
    binding
        .authorization_for("https://api.example.test/resource")
        .await
        .unwrap();

    let outcome = binding
        .report_rejected_for_response("https://api.example.test/resource", 401, None)
        .await;

    assert!(matches!(outcome, RefreshOutcome::RefreshFailed(_)));
    assert_eq!(
        store
            .get(&id)
            .unwrap()
            .unwrap()
            .access_token()
            .expose_secret(),
        "old-access"
    );
    assert!(matches!(
        binding.health(),
        crate::CredentialHealth::NeedsConsent
    ));
    assert!(matches!(
        binding
            .authorization_for("https://api.example.test/resource")
            .await,
        Err(crate::AuthUnavailable::NeedsConsent)
    ));
}

fn loopback_login_request(scheme: OauthScheme) -> crate::request::LoopbackLoginRequest {
    let LoginRequest::Loopback(request) = OAuthRequest::new(scheme).into_login_request() else {
        panic!("expected loopback login request");
    };
    request
}

fn manual_code_login_request(scheme: OauthScheme) -> crate::request::ManualCodeLoginRequest {
    let LoginRequest::ManualCode(request) = OAuthRequest::new(scheme).into_login_request() else {
        panic!("expected manual-code login request");
    };
    request
}

fn client_side_token_login_request(scheme: OauthScheme) -> ClientSideTokenLoginRequest {
    let LoginRequest::ClientSideToken(request) = OAuthRequest::new(scheme).into_login_request()
    else {
        panic!("expected client-side token login request");
    };
    request
}

fn device_code_login_request(scheme: OauthScheme) -> crate::request::DeviceCodeLoginRequest {
    let LoginRequest::DeviceCode(request) = OAuthRequest::new(scheme).into_login_request() else {
        panic!("expected device-code login request");
    };
    request
}

fn binding_with_oauth(
    scheme: OauthScheme,
) -> (Arc<AuthBinding>, Arc<dyn CredentialStore>, CredentialId) {
    let store: Arc<dyn CredentialStore> = Arc::new(MemoryStore::default());
    let http = reqwest::ClientBuilder::new()
        .danger_accept_invalid_certs(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let service = Arc::new(CredentialService::new(
        Arc::clone(&store),
        OAuthClient::from_http_client(http),
    ));
    let id = CredentialId::new("test-provider", "oauth", "default").unwrap();
    seed_oauth(store.as_ref(), &id, "old-access", "refresh-1", 3600);
    let binding = service
        .bind_oauth(
            id.clone(),
            OAuthRequest::new(scheme),
            vec!["api.example.test".to_owned()],
            "Authorization".to_owned(),
            "Bearer ".to_owned(),
        )
        .unwrap();
    (Arc::new(binding), store, id)
}

fn binding_scheme() -> OauthScheme {
    OauthScheme {
        key: "oauth".to_owned(),
        display_name: "test oauth".to_owned(),
        authorization_endpoint: "https://auth.example.test/authorize".to_owned(),
        token_endpoint: "https://auth.example.test/token".to_owned(),
        revocation_endpoint: None,
        default_client_id: Some("client-id".to_owned()),
        default_scopes: vec!["read".to_owned()],
        flow: OAuthFlow::PkceManualCode(PkceManualCodeConfig {
            redirect_uri: "https://localhost/callback".to_owned(),
        }),
        token_endpoint_auth: TokenEndpointAuthMethod::None,
        refresh_token_rotates: true,
        extra_authorize_params: vec![],
        extra_token_params: vec![],
        inject_domains: vec!["api.example.test".to_owned()],
        inject_header_name: None,
        inject_value_prefix: "Bearer ".to_owned(),
    }
}

fn seed_oauth(
    store: &dyn CredentialStore,
    id: &CredentialId,
    access_token: &str,
    refresh_token: &str,
    expires_in_seconds: i64,
) {
    store
        .put(
            id,
            &CredentialEntry::oauth(
                SecretString::from(access_token.to_owned()),
                Some(SecretString::from(refresh_token.to_owned())),
                Some(OffsetDateTime::now_utc() + time::Duration::seconds(expires_in_seconds)),
                "Bearer",
                vec!["read".to_owned()],
                OffsetDateTime::now_utc(),
            ),
        )
        .unwrap();
}
