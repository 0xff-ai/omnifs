use crate::callback::{LoopbackCallback, LoopbackEndpoint, accept_callback_request};
use crate::request::ClientSideTokenLoginRequest;
use crate::test_support::{FakeAuthServer, FakeBehavior, FakeOpener, FakeRevocationServer};
use crate::{
    AuthBinding, AuthError, CredentialService, LoginRequest, OAuthClient, OAuthRequest,
    OAuthRequestConfig, OAuthRevokeOutcome, RefreshOutcome, UrlOpener,
};
use crate::{
    CredentialEntry, DurableCredentialSnapshot, RefreshCandidate, RefreshClassification,
    RefreshPersistError, RefreshPersistence, RefreshSink,
};
use crate::{
    CredentialId, DevicePollCompat, OAuthFlow, OauthScheme, PkceManualCodeConfig,
    TokenEndpointAuthMethod,
};
use omnifs_core::CredentialVersion;
use secrecy::{ExposeSecret, SecretString};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
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
    let client = OAuthClient::from_http_client(http);

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

    let failing = FakeRevocationServer::start_with_failure(true).await;
    let error = client
        .revoke_access_token(
            OAuthRequest::new(fake.loopback_scheme(Some(failing.endpoint()))),
            SecretString::from("access-3".to_owned()),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, AuthError::RevocationEndpoint(_)));
}

#[tokio::test]
async fn refresh_exchange_parses_rotated_refresh_token() {
    let fake = FakeAuthServer::start(FakeBehavior::default()).await;
    let scheme = fake.loopback_scheme(None);
    let client = OAuthClient::new().unwrap();
    let mut current = refreshable_entry(vec!["old-scope".to_owned()]);
    current.set_upstream_identity(Some("user@example.test".to_owned()));

    let entry = client
        .refresh(OAuthRequest::new(scheme), &current)
        .await
        .unwrap();

    assert_eq!(entry.access_token().expose_secret(), "access-refresh-1");
    assert_eq!(
        entry.refresh_token().unwrap().expose_secret(),
        "refresh-rotated-1"
    );
    assert_eq!(entry.scopes(), ["read", "write"]);
    assert_eq!(entry.upstream_identity(), Some("user@example.test"));
}

#[tokio::test]
async fn device_code_refresh_does_not_require_redirect_uri() {
    let fake = FakeAuthServer::start(FakeBehavior::default()).await;
    let scheme = fake.device_scheme(DevicePollCompat::Rfc8628, None);
    let client = OAuthClient::new().unwrap();
    let current = refreshable_entry(vec!["read".to_owned()]);

    let entry = client
        .refresh(OAuthRequest::new(scheme), &current)
        .await
        .unwrap();

    assert_eq!(entry.access_token().expose_secret(), "access-refresh-1");
}

#[test]
fn refresh_merge_retains_omitted_refresh_token_scopes_and_identity() {
    let mut current = refreshable_entry(vec!["read".to_owned(), "write".to_owned()]);
    current.set_upstream_identity(Some("user@example.test".to_owned()));
    let token = serde_json::from_value(serde_json::json!({
        "access_token": "new-access",
        "expires_in": 1800,
        "token_type": "Bearer",
    }))
    .unwrap();

    let entry = crate::flows::credential_entry_from_refresh_token(&token, &current);

    assert_eq!(entry.access_token().expose_secret(), "new-access");
    assert_eq!(entry.refresh_token().unwrap().expose_secret(), "refresh-1");
    assert_eq!(entry.scopes(), ["read", "write"]);
    assert_eq!(entry.upstream_identity(), Some("user@example.test"));
    assert_eq!(entry.token_type(), "bearer");
    assert!(entry.expires_at().is_some());
}

#[test]
fn refresh_merge_honors_explicit_empty_scopes() {
    let current = refreshable_entry(vec!["read".to_owned()]);
    let mut token = oauth2::StandardTokenResponse::new(
        oauth2::AccessToken::new("new-access".to_owned()),
        oauth2::basic::BasicTokenType::Bearer,
        oauth2::EmptyExtraTokenFields {},
    );
    token.set_scopes(Some(Vec::new()));

    let entry = crate::flows::credential_entry_from_refresh_token(&token, &current);

    assert!(entry.scopes().is_empty());
}

#[test]
fn refresh_merge_normalizes_empty_wire_scope_to_explicit_empty_set() {
    let current = refreshable_entry(vec!["read".to_owned()]);
    let token = serde_json::from_value(serde_json::json!({
        "access_token": "new-access",
        "token_type": "Bearer",
        "scope": "",
    }))
    .unwrap();

    let entry = crate::flows::credential_entry_from_refresh_token(&token, &current);

    assert!(entry.scopes().is_empty());
}

#[tokio::test]
async fn authorization_refreshes_credential_inside_refresh_window() {
    let fake = FakeAuthServer::start(FakeBehavior::default()).await;
    let (binding, _) = binding_with_oauth(fake.loopback_scheme(None), 30);

    let authorization = binding
        .authorization_for("https://api.example.test/resource")
        .await
        .unwrap();

    assert_eq!(authorization.unwrap().1, "Bearer access-refresh-1");
    assert_eq!(fake.refreshes(), 1);
}

#[tokio::test]
async fn authorization_keeps_credential_outside_refresh_window() {
    let fake = FakeAuthServer::start(FakeBehavior::default()).await;
    let (binding, _) = binding_with_oauth(fake.loopback_scheme(None), 3600);

    let authorization = binding
        .authorization_for("https://api.example.test/resource")
        .await
        .unwrap();

    assert_eq!(authorization.unwrap().1, "Bearer old-access");
    assert_eq!(fake.refreshes(), 0);
}

#[tokio::test]
async fn binding_rejection_single_flights_refresh() {
    let fake = FakeAuthServer::start(FakeBehavior {
        refresh_delay_ms: 50,
        ..FakeBehavior::default()
    })
    .await;
    let (binding, _) = binding_with_oauth(fake.loopback_scheme(None), 3600);
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
        binding
            .current_for_test()
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
    let (binding, _) = binding_with_oauth(fake.loopback_scheme(None), 3600);
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
    let (binding, _) = binding_with_oauth(fake.loopback_scheme(None), 3600);
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
    let id = CredentialId::new("test-provider", "oauth", "default").unwrap();
    let service = Arc::new(durable_service(
        &id,
        refreshable_entry(vec!["read".to_owned()]),
        Arc::new(SimpleRefreshSink),
    ));

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
    let conflicting_request = OAuthRequest::from_config(
        Some(&OAuthRequestConfig {
            client_id: Some("different-client".to_owned()),
            ..OAuthRequestConfig::default()
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
async fn shared_binding_adopts_fresh_snapshot_without_second_refresh() {
    let fake = FakeAuthServer::start(FakeBehavior::default()).await;
    let id = CredentialId::new("test-provider", "oauth", "default").unwrap();
    let http = reqwest::ClientBuilder::new()
        .danger_accept_invalid_certs(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let service = Arc::new(CredentialService::new(
        [(
            id.clone(),
            DurableCredentialSnapshot {
                entry: oauth_entry_for_seconds(3600),
                version: CredentialVersion::initial(),
            },
        )],
        OAuthClient::from_http_client(http),
        Arc::new(SimpleRefreshSink),
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
fn bind_rejects_snapshot_kind_mismatch() {
    let id = CredentialId::new("test-provider", "pat", "default").unwrap();
    let service = Arc::new(durable_service(
        &id,
        CredentialEntry::oauth(SecretString::from("access"), None, None, "Bearer", vec![]),
        Arc::new(SimpleRefreshSink),
    ));

    assert!(matches!(
        service.bind_static(id.clone(), vec![], "Authorization".to_owned(), "Bearer ".to_owned()),
        Err(AuthError::CredentialKindMismatch { id: error_id, .. }) if error_id == id
    ));
}

#[tokio::test]
async fn invalid_grant_refresh_needs_consent_and_keeps_snapshot() {
    let fake = FakeAuthServer::start(FakeBehavior {
        token_error: Some(("invalid_grant".to_owned(), "revoked".to_owned())),
        ..FakeBehavior::default()
    })
    .await;
    let (binding, _) = binding_with_oauth(fake.loopback_scheme(None), 3600);
    binding
        .authorization_for("https://api.example.test/resource")
        .await
        .unwrap();

    let outcome = binding
        .report_rejected_for_response("https://api.example.test/resource", 401, None)
        .await;

    assert!(matches!(outcome, RefreshOutcome::RefreshFailed(_)));
    assert_eq!(
        binding
            .current_for_test()
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

#[tokio::test]
async fn durable_routine_refresh_persists_before_binding_exposes_token() {
    let fake = FakeAuthServer::start(FakeBehavior::default()).await;
    let id = CredentialId::new("test-provider", "oauth", "default").unwrap();
    let entry = durable_test_entry(vec!["read".to_owned(), "write".to_owned()]);
    let (candidate_tx, mut candidate_rx) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Notify::new());
    let sink = Arc::new(TestRefreshSink::new(
        vec![Ok(RefreshPersistence::Active {
            version: CredentialVersion::new(std::num::NonZeroU64::new(2).unwrap()),
        })],
        candidate_tx,
        Some(Arc::clone(&release)),
    ));
    let service = Arc::new(durable_service(&id, entry, sink.clone()));
    let binding = Arc::new(
        service
            .bind_oauth(
                id.clone(),
                OAuthRequest::new(fake.loopback_scheme(None)),
                vec!["api.example.test".to_owned()],
                "Authorization".to_owned(),
                "Bearer ".to_owned(),
            )
            .unwrap(),
    );

    let request = {
        let binding = Arc::clone(&binding);
        tokio::spawn(async move {
            binding
                .authorization_for("https://api.example.test/resource")
                .await
        })
    };
    let candidate = candidate_rx.recv().await.unwrap();
    assert_eq!(candidate.credential_id, id);
    assert_eq!(candidate.expected_version, CredentialVersion::initial());
    assert_eq!(candidate.classification, RefreshClassification::Routine);
    assert_eq!(
        binding
            .current_for_test()
            .unwrap()
            .access_token()
            .expose_secret(),
        "old-access"
    );

    release.notify_one();
    let authorization = request.await.unwrap().unwrap().unwrap();
    assert_eq!(authorization.1, "Bearer access-refresh-1");
    assert_eq!(sink.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn durable_authority_change_stays_hidden_and_returns_pending() {
    let fake = FakeAuthServer::start(FakeBehavior::default()).await;
    let id = CredentialId::new("test-provider", "oauth", "default").unwrap();
    let entry = durable_test_entry(vec!["read".to_owned()]);
    let (candidate_tx, mut candidate_rx) = tokio::sync::mpsc::unbounded_channel();
    let sink = Arc::new(TestRefreshSink::new(
        vec![Ok(RefreshPersistence::PendingRepublish {
            version: CredentialVersion::new(std::num::NonZeroU64::new(2).unwrap()),
        })],
        candidate_tx,
        None,
    ));
    let service = Arc::new(durable_service(&id, entry, sink.clone()));
    let binding = service
        .bind_oauth(
            id.clone(),
            OAuthRequest::new(fake.loopback_scheme(None)),
            vec!["api.example.test".to_owned()],
            "Authorization".to_owned(),
            "Bearer ".to_owned(),
        )
        .unwrap();

    let error = binding
        .authorization_for("https://api.example.test/resource")
        .await
        .unwrap_err();
    assert!(matches!(error, crate::AuthUnavailable::RefreshPending));
    let candidate = candidate_rx.recv().await.unwrap();
    assert_eq!(
        candidate.classification,
        RefreshClassification::AuthorityChanged
    );
    assert_eq!(
        binding
            .current_for_test()
            .unwrap()
            .access_token()
            .expose_secret(),
        "old-access"
    );
    assert!(matches!(
        binding
            .authorization_for("https://api.example.test/resource")
            .await,
        Err(crate::AuthUnavailable::RefreshPending)
    ));
    assert!(matches!(
        binding
            .report_rejected_for_response("https://api.example.test/resource", 401, None)
            .await,
        RefreshOutcome::RefreshFailed(message)
            if message == crate::AuthUnavailable::RefreshPending.to_string()
    ));
    assert_eq!(sink.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn durable_refresh_failure_keeps_old_snapshot() {
    let fake = FakeAuthServer::start(FakeBehavior::default()).await;
    let id = CredentialId::new("test-provider", "oauth", "default").unwrap();
    let entry = durable_test_entry(vec!["read".to_owned(), "write".to_owned()]);
    let (candidate_tx, mut candidate_rx) = tokio::sync::mpsc::unbounded_channel();
    let sink = Arc::new(TestRefreshSink::new(
        vec![Err(RefreshPersistError::Unavailable)],
        candidate_tx,
        None,
    ));
    let service = Arc::new(durable_service(&id, entry, sink));
    let binding = service
        .bind_oauth(
            id.clone(),
            OAuthRequest::new(fake.loopback_scheme(None)),
            vec!["api.example.test".to_owned()],
            "Authorization".to_owned(),
            "Bearer ".to_owned(),
        )
        .unwrap();

    assert!(matches!(
        binding
            .authorization_for("https://api.example.test/resource")
            .await,
        Err(crate::AuthUnavailable::RefreshFailed(_))
    ));
    let first = candidate_rx.recv().await.unwrap();
    assert_eq!(first.expected_version, CredentialVersion::initial());
    assert_eq!(
        binding
            .current_for_test()
            .unwrap()
            .access_token()
            .expose_secret(),
        "old-access"
    );
}

#[tokio::test]
async fn durable_refresh_advances_cas_version_after_active_ack() {
    let fake = FakeAuthServer::start(FakeBehavior::default()).await;
    let id = CredentialId::new("test-provider", "oauth", "default").unwrap();
    let entry = durable_test_entry(vec!["read".to_owned(), "write".to_owned()]);
    let (candidate_tx, mut candidate_rx) = tokio::sync::mpsc::unbounded_channel();
    let sink = Arc::new(TestRefreshSink::new(
        vec![Ok(RefreshPersistence::Active {
            version: CredentialVersion::new(std::num::NonZeroU64::new(2).unwrap()),
        })],
        candidate_tx,
        None,
    ));
    let service = Arc::new(durable_service(&id, entry, sink));
    let binding = service
        .bind_oauth(
            id.clone(),
            OAuthRequest::new(fake.loopback_scheme(None)),
            vec!["api.example.test".to_owned()],
            "Authorization".to_owned(),
            "Bearer ".to_owned(),
        )
        .unwrap();
    assert!(
        binding
            .authorization_for("https://api.example.test/resource")
            .await
            .is_ok()
    );
    assert_eq!(
        candidate_rx.recv().await.unwrap().expected_version,
        CredentialVersion::initial()
    );
    assert_eq!(
        service.version_for_test(&id),
        Some(CredentialVersion::new(
            std::num::NonZeroU64::new(2).unwrap(),
        ))
    );
}

#[tokio::test]
async fn durable_refresh_singleflight_persists_once() {
    let fake = FakeAuthServer::start(FakeBehavior {
        refresh_delay_ms: 50,
        ..FakeBehavior::default()
    })
    .await;
    let id = CredentialId::new("test-provider", "oauth", "default").unwrap();
    let entry = durable_test_entry(vec!["read".to_owned(), "write".to_owned()]);
    let (candidate_tx, mut candidate_rx) = tokio::sync::mpsc::unbounded_channel();
    let sink = Arc::new(TestRefreshSink::new(
        vec![Ok(RefreshPersistence::Active {
            version: CredentialVersion::new(std::num::NonZeroU64::new(2).unwrap()),
        })],
        candidate_tx,
        None,
    ));
    let service = Arc::new(durable_service(&id, entry, sink));
    let binding = Arc::new(
        service
            .bind_oauth(
                id,
                OAuthRequest::new(fake.loopback_scheme(None)),
                vec!["api.example.test".to_owned()],
                "Authorization".to_owned(),
                "Bearer ".to_owned(),
            )
            .unwrap(),
    );

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
    for handle in handles {
        assert_eq!(handle.await.unwrap(), RefreshOutcome::Refreshed);
    }
    let candidate = candidate_rx.recv().await.unwrap();
    assert_eq!(candidate.expected_version, CredentialVersion::initial());
    assert!(candidate_rx.try_recv().is_err());
    assert_eq!(fake.refreshes(), 1);
}

#[tokio::test]
async fn durable_snapshots_keep_independent_updates_for_concurrent_credentials() {
    let fake = FakeAuthServer::start(FakeBehavior::default()).await;
    let first_id = CredentialId::new("test-provider", "oauth", "first").unwrap();
    let second_id = CredentialId::new("test-provider", "oauth", "second").unwrap();
    let (candidate_tx, mut candidate_rx) = tokio::sync::mpsc::unbounded_channel();
    let sink = Arc::new(BarrierRefreshSink {
        barrier: Arc::new(tokio::sync::Barrier::new(2)),
        candidates: candidate_tx,
        calls: AtomicUsize::new(0),
    });
    let service = Arc::new(durable_service_with_snapshots(
        [
            (
                first_id.clone(),
                DurableCredentialSnapshot {
                    entry: durable_test_entry(vec!["read".to_owned(), "write".to_owned()]),
                    version: CredentialVersion::initial(),
                },
            ),
            (
                second_id.clone(),
                DurableCredentialSnapshot {
                    entry: durable_test_entry(vec!["read".to_owned(), "write".to_owned()]),
                    version: CredentialVersion::initial(),
                },
            ),
        ],
        sink,
    ));
    let first = Arc::new(
        service
            .bind_oauth(
                first_id.clone(),
                OAuthRequest::new(fake.loopback_scheme(None)),
                vec!["api.example.test".to_owned()],
                "Authorization".to_owned(),
                "Bearer ".to_owned(),
            )
            .unwrap(),
    );
    let second = Arc::new(
        service
            .bind_oauth(
                second_id.clone(),
                OAuthRequest::new(fake.loopback_scheme(None)),
                vec!["api.example.test".to_owned()],
                "Authorization".to_owned(),
                "Bearer ".to_owned(),
            )
            .unwrap(),
    );

    let (first_result, second_result) = tokio::join!(
        first.authorization_for("https://api.example.test/resource"),
        second.authorization_for("https://api.example.test/resource")
    );
    assert!(first_result.is_ok());
    assert!(second_result.is_ok());
    let mut first_candidates = [
        candidate_rx.recv().await.unwrap(),
        candidate_rx.recv().await.unwrap(),
    ];
    first_candidates.sort_by_key(|candidate| candidate.credential_id.to_string());
    assert_eq!(
        first_candidates[0].expected_version,
        CredentialVersion::initial()
    );
    assert_eq!(
        first_candidates[1].expected_version,
        CredentialVersion::initial()
    );

    assert_eq!(
        service
            .version_for_test(&first_id)
            .map(CredentialVersion::get),
        Some(2)
    );
    assert_eq!(
        service
            .version_for_test(&second_id)
            .map(CredentialVersion::get),
        Some(2)
    );
}

struct TestRefreshSink {
    responses: tokio::sync::Mutex<Vec<Result<RefreshPersistence, RefreshPersistError>>>,
    candidates: tokio::sync::mpsc::UnboundedSender<RefreshCandidate>,
    release: Option<Arc<tokio::sync::Notify>>,
    calls: AtomicUsize,
}

struct BarrierRefreshSink {
    barrier: Arc<tokio::sync::Barrier>,
    candidates: tokio::sync::mpsc::UnboundedSender<RefreshCandidate>,
    calls: AtomicUsize,
}

impl RefreshSink for BarrierRefreshSink {
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
            let version = candidate
                .expected_version
                .next()
                .expect("test credential version has room to advance");
            self.candidates
                .send(candidate)
                .expect("refresh candidate receiver remains open");
            self.barrier.wait().await;
            Ok(RefreshPersistence::Active { version })
        })
    }
}

impl TestRefreshSink {
    fn new(
        responses: Vec<Result<RefreshPersistence, RefreshPersistError>>,
        candidates: tokio::sync::mpsc::UnboundedSender<RefreshCandidate>,
        release: Option<Arc<tokio::sync::Notify>>,
    ) -> Self {
        Self {
            responses: tokio::sync::Mutex::new(responses),
            candidates,
            release,
            calls: AtomicUsize::new(0),
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
            self.candidates
                .send(candidate)
                .expect("refresh candidate receiver remains open");
            if let Some(release) = &self.release {
                release.notified().await;
            }
            let mut responses = self.responses.lock().await;
            assert!(!responses.is_empty(), "test sink has a response");
            responses.remove(0)
        })
    }
}

fn durable_service(
    id: &CredentialId,
    entry: CredentialEntry,
    sink: Arc<dyn RefreshSink>,
) -> CredentialService {
    durable_service_with_snapshots(
        [(
            id.to_owned(),
            DurableCredentialSnapshot {
                entry,
                version: CredentialVersion::initial(),
            },
        )],
        sink,
    )
}

fn durable_service_with_snapshots(
    snapshots: impl IntoIterator<Item = (CredentialId, DurableCredentialSnapshot)>,
    sink: Arc<dyn RefreshSink>,
) -> CredentialService {
    let http = reqwest::ClientBuilder::new()
        .danger_accept_invalid_certs(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    CredentialService::new(snapshots, OAuthClient::from_http_client(http), sink)
}

fn durable_test_entry(scopes: Vec<String>) -> CredentialEntry {
    CredentialEntry::oauth(
        SecretString::from("old-access".to_owned()),
        Some(SecretString::from("refresh-1".to_owned())),
        Some(OffsetDateTime::now_utc() + time::Duration::seconds(30)),
        "Bearer",
        scopes,
    )
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
    expires_in_seconds: i64,
) -> (Arc<AuthBinding>, CredentialId) {
    let id = CredentialId::new("test-provider", "oauth", "default").unwrap();
    let http = reqwest::ClientBuilder::new()
        .danger_accept_invalid_certs(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let service = Arc::new(CredentialService::new(
        [(
            id.clone(),
            DurableCredentialSnapshot {
                entry: oauth_entry_for_seconds(expires_in_seconds),
                version: CredentialVersion::initial(),
            },
        )],
        OAuthClient::from_http_client(http),
        Arc::new(SimpleRefreshSink),
    ));
    let binding = service
        .bind_oauth(
            id.clone(),
            OAuthRequest::new(scheme),
            vec!["api.example.test".to_owned()],
            "Authorization".to_owned(),
            "Bearer ".to_owned(),
        )
        .unwrap();
    (Arc::new(binding), id)
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

fn oauth_entry_for_seconds(expires_in_seconds: i64) -> CredentialEntry {
    CredentialEntry::oauth(
        SecretString::from("old-access".to_owned()),
        Some(SecretString::from("refresh-1".to_owned())),
        Some(OffsetDateTime::now_utc() + time::Duration::seconds(expires_in_seconds)),
        "Bearer",
        vec!["read".to_owned(), "write".to_owned()],
    )
}

struct SimpleRefreshSink;

impl RefreshSink for SimpleRefreshSink {
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
            let version = candidate
                .expected_version
                .next()
                .expect("test credential version has room to advance");
            Ok(RefreshPersistence::Active { version })
        })
    }
}

fn refreshable_entry(scopes: Vec<String>) -> CredentialEntry {
    CredentialEntry::oauth(
        SecretString::from("old-access".to_owned()),
        Some(SecretString::from("refresh-1".to_owned())),
        Some(OffsetDateTime::now_utc() + time::Duration::hours(1)),
        "Bearer",
        scopes,
    )
}
