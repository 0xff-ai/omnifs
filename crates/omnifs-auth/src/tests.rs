use crate::callback::{LoopbackCallback, LoopbackEndpoint, accept_callback_request};
use crate::request::ClientSideTokenLoginRequest;
use crate::test_support::{FakeAuthServer, FakeBehavior, FakeOpener, FakeRevocationServer};
use crate::{
    AuthError, CredentialHealth, CredentialService, LoginRequest, OAuthClient, OAuthRequest,
    OAuthRevokeOutcome, RefreshOutcome, RejectionEvidence, RevokeOutcome, UrlOpener,
};
use omnifs_workspace::authn::{CredentialId, DevicePollCompat, OauthScheme};
use omnifs_workspace::creds::{CredentialEntry, CredentialStore, MemoryStore, Refreshability};
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
async fn optional_revocation_endpoint_works_without_builder_type_branching() {
    let fake = FakeAuthServer::start(FakeBehavior::default()).await;
    let revoke_fake = FakeRevocationServer::start().await;
    let scheme = fake.loopback_scheme(Some(revoke_fake.endpoint()));
    let http = reqwest::ClientBuilder::new()
        .danger_accept_invalid_certs(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let client = OAuthClient::new().unwrap().with_http_client(http);

    let revoked = client
        .revoke_access_token(
            OAuthRequest::new(scheme),
            SecretString::from("access-1".to_owned()),
        )
        .await
        .unwrap();

    assert_eq!(revoked, OAuthRevokeOutcome::Revoked);
    assert_eq!(revoke_fake.revocations(), 1);

    let no_revoke_scheme = fake.loopback_scheme(None);
    let skipped = client
        .revoke_access_token(
            OAuthRequest::new(no_revoke_scheme),
            SecretString::from("access-2".to_owned()),
        )
        .await
        .unwrap();
    assert_eq!(skipped, OAuthRevokeOutcome::Unsupported);
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
async fn report_rejected_401_single_flights_refresh_and_updates_health() {
    let fake = FakeAuthServer::start(FakeBehavior {
        refresh_delay_ms: 50,
        ..FakeBehavior::default()
    })
    .await;
    let (service, store, id) = service_with_oauth(fake.loopback_scheme(None));
    seed_oauth(store.as_ref(), &id, "old-access", "refresh-1", 3600);
    service.authorization(&id).await.unwrap();

    // Collected eagerly so all tasks are spawned (and observe the pre-rotation
    // token) before any is awaited; a lazy iterator would serialize them.
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let service = Arc::clone(&service);
            let id = id.clone();
            tokio::spawn(async move {
                service
                    .report_rejected(&id, RejectionEvidence::new(401, None))
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
    assert!(
        service
            .health()
            .into_iter()
            .any(|status| { status.id == id && matches!(status.health, CredentialHealth::Ready) })
    );
}

#[tokio::test]
async fn report_rejected_403_bearer_invalid_token_refreshes() {
    let fake = FakeAuthServer::start(FakeBehavior::default()).await;
    let (service, store, id) = service_with_oauth(fake.loopback_scheme(None));
    seed_oauth(store.as_ref(), &id, "old-access", "refresh-1", 3600);
    service.authorization(&id).await.unwrap();

    let outcome = service
        .report_rejected(
            &id,
            RejectionEvidence::new(
                403,
                Some(r#"Bearer realm="api", error="invalid_token""#.to_owned()),
            ),
        )
        .await;

    assert_eq!(outcome, RefreshOutcome::Refreshed);
    assert_eq!(fake.refreshes(), 1);
}

#[tokio::test]
async fn report_rejected_403_unrelated_challenge_does_not_refresh() {
    let fake = FakeAuthServer::start(FakeBehavior::default()).await;
    let (service, store, id) = service_with_oauth(fake.loopback_scheme(None));
    seed_oauth(store.as_ref(), &id, "old-access", "refresh-1", 3600);
    service.authorization(&id).await.unwrap();

    let outcome = service
        .report_rejected(
            &id,
            RejectionEvidence::new(403, Some(r#"Bearer error="not_invalid_token""#.to_owned())),
        )
        .await;

    assert_eq!(outcome, RefreshOutcome::NotApplicable);
    assert_eq!(fake.refreshes(), 0);
}

#[tokio::test]
async fn invalid_grant_refresh_needs_consent_and_keeps_stored_entry() {
    let fake = FakeAuthServer::start(FakeBehavior {
        token_error: Some(("invalid_grant".to_owned(), "revoked".to_owned())),
        ..FakeBehavior::default()
    })
    .await;
    let (service, store, id) = service_with_oauth(fake.loopback_scheme(None));
    seed_oauth(store.as_ref(), &id, "old-access", "refresh-1", 3600);
    service.authorization(&id).await.unwrap();

    let outcome = service
        .report_rejected(&id, RejectionEvidence::new(401, None))
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
    assert!(service.health().into_iter().any(|status| {
        status.id == id && matches!(status.health, CredentialHealth::NeedsConsent)
    }));
    assert!(matches!(
        service.authorization(&id).await,
        Err(crate::AuthUnavailable::NeedsConsent)
    ));
}

#[tokio::test]
async fn needs_consent_credential_leaves_the_refresh_schedule() {
    let fake = FakeAuthServer::start(FakeBehavior {
        token_error: Some(("invalid_grant".to_owned(), "revoked".to_owned())),
        ..FakeBehavior::default()
    })
    .await;
    let (service, store, id) = service_with_oauth(fake.loopback_scheme(None));
    seed_oauth(store.as_ref(), &id, "old-access", "refresh-1", 3600);
    service.authorization(&id).await.unwrap();
    assert!(service.earliest_oauth_deadline().is_some());

    service
        .report_rejected(&id, RejectionEvidence::new(401, None))
        .await;

    // A NeedsConsent credential's past-due deadline must not pin the loop's
    // minimum, or it starves every other credential of proactive refresh.
    assert!(service.earliest_oauth_deadline().is_none());
}

#[tokio::test]
async fn revoke_and_delete_revokes_access_token_then_deletes_local_entry() {
    let fake = FakeAuthServer::start(FakeBehavior::default()).await;
    let revoke_fake = FakeRevocationServer::start().await;
    let (service, store, id) =
        service_with_oauth(fake.loopback_scheme(Some(revoke_fake.endpoint())));
    seed_oauth(store.as_ref(), &id, "access-1", "refresh-1", 3600);

    let outcome = service.revoke_and_delete(&id).await;

    assert_eq!(outcome, RevokeOutcome::Revoked);
    assert_eq!(revoke_fake.revoked_tokens(), ["access-1"]);
    assert!(store.get(&id).unwrap().is_none());
}

#[tokio::test]
async fn revoke_and_delete_deletes_local_entry_when_upstream_revoke_fails() {
    let fake = FakeAuthServer::start(FakeBehavior::default()).await;
    let revoke_fake = FakeRevocationServer::start_with_failure(true).await;
    let (service, store, id) =
        service_with_oauth(fake.loopback_scheme(Some(revoke_fake.endpoint())));
    seed_oauth(store.as_ref(), &id, "access-1", "refresh-1", 3600);

    let outcome = service.revoke_and_delete(&id).await;

    assert!(matches!(outcome, RevokeOutcome::Failed { .. }));
    assert_eq!(revoke_fake.revoked_tokens(), ["access-1"]);
    assert!(store.get(&id).unwrap().is_none());
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

fn service_with_oauth(
    scheme: OauthScheme,
) -> (
    Arc<CredentialService>,
    Arc<dyn CredentialStore>,
    CredentialId,
) {
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
    service.register_oauth(id.clone(), OAuthRequest::new(scheme));
    (service, store, id)
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
