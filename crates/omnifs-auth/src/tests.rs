use crate::callback::{LoopbackCallback, LoopbackEndpoint, accept_callback_request};
use crate::request::ClientSideTokenLoginRequest;
use crate::test_support::{FakeAuthServer, FakeBehavior, FakeOpener, FakeRevocationServer};
use crate::{AuthError, LoginRequest, OAuthClient, OAuthRequest, RevokeOutcome, UrlOpener};
use omnifs_workspace::authn::OauthScheme;
use omnifs_workspace::creds::Refreshability;
use secrecy::{ExposeSecret, SecretString};
use std::sync::Arc;
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
    let scheme = fake.device_scheme(None);
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
    let scheme = fake.device_scheme(None);
    let client = OAuthClient::new().unwrap();

    let entry = client
        .login_device_code(device_code_login_request(scheme), |_| async { Ok(()) })
        .await
        .unwrap();

    assert_eq!(entry.access_token().expose_secret(), "device-access-1");
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

    assert_eq!(revoked, RevokeOutcome::Revoked);
    assert_eq!(revoke_fake.revocations(), 1);

    let no_revoke_scheme = fake.loopback_scheme(None);
    let skipped = client
        .revoke_access_token(
            OAuthRequest::new(no_revoke_scheme),
            SecretString::from("access-2".to_owned()),
        )
        .await
        .unwrap();
    assert_eq!(skipped, RevokeOutcome::Unsupported);
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
    let scheme = fake.device_scheme(None);
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
