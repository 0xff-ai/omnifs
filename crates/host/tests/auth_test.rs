#![allow(unsafe_code)]

use omnifs_host::auth::AuthManager;
use omnifs_host::config::AuthConfig;
use omnifs_host::omnifs::provider::types as wit_types;
use omnifs_host::runtime::capability::{CapabilityChecker, CapabilityGrants};
use omnifs_host::runtime::executor::HttpExecutor;
use std::ffi::OsString;
use std::sync::Arc;
use std::sync::{LazyLock, Mutex, MutexGuard};

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

#[test]
fn test_bearer_token_injection() {
    let auth = AuthConfig {
        auth_type: "bearer-token".to_string(),
        token_env: Some("OMNIFS_TEST_TOKEN_AUTH".to_string()),
        token_file: None,
        domain: None,
        header: None,
        scopes: None,
    };
    let _env = ScopedEnvVar::set("OMNIFS_TEST_TOKEN_AUTH", "ghp_test123");
    let manager = AuthManager::from_config(&auth).unwrap();
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
    let auth = AuthConfig {
        auth_type: "bearer-token".to_string(),
        token_env: Some("DEFINITELY_NOT_SET_12345".to_string()),
        token_file: None,
        domain: None,
        header: None,
        scopes: None,
    };
    let manager = AuthManager::from_config(&auth).unwrap();
    let headers = manager.headers_for_url("https://api.github.com/repos");
    assert!(headers.is_empty());
    assert!(manager.requires_auth_for_url("https://api.github.com/repos"));
}

#[test]
fn test_bearer_token_injection_from_file() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = dir.path().join("github_token");
    std::fs::write(&token_file, "ghp_file_token\n").unwrap();
    let auth = AuthConfig {
        auth_type: "bearer-token".to_string(),
        token_env: None,
        token_file: Some(token_file.display().to_string()),
        domain: None,
        header: None,
        scopes: None,
    };
    let manager = AuthManager::from_config(&auth).unwrap();
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
    let auth = AuthConfig {
        auth_type: "bearer-token".to_string(),
        token_env: Some("OMNIFS_TEST_TOKEN_AUTH_PREFERRED".to_string()),
        token_file: Some(token_file.display().to_string()),
        domain: None,
        header: None,
        scopes: None,
    };
    let _env = ScopedEnvVar::set("OMNIFS_TEST_TOKEN_AUTH_PREFERRED", "ghp_from_env");
    let manager = AuthManager::from_config(&auth).unwrap();
    let headers = manager.headers_for_url("https://api.github.com/repos");
    assert_eq!(headers[0].1, "Bearer ghp_from_file");
}

#[test]
fn test_missing_token_file_falls_back_to_env() {
    let dir = tempfile::tempdir().unwrap();
    let missing_token_file = dir.path().join("missing_token");
    let auth = AuthConfig {
        auth_type: "bearer-token".to_string(),
        token_env: Some("OMNIFS_TEST_TOKEN_AUTH_FALLBACK".to_string()),
        token_file: Some(missing_token_file.display().to_string()),
        domain: None,
        header: None,
        scopes: None,
    };
    let _env = ScopedEnvVar::set("OMNIFS_TEST_TOKEN_AUTH_FALLBACK", "ghp_from_env");
    let manager = AuthManager::from_config(&auth).unwrap();
    let headers = manager.headers_for_url("https://api.github.com/repos");
    assert_eq!(headers[0].1, "Bearer ghp_from_env");
}

#[tokio::test]
async fn test_execute_fetch_returns_denied_when_auth_is_required_but_missing() {
    // Create an AuthManager with a config that requires auth for api.github.com
    // but has no valid credential (env var doesn't exist). The injector should
    // exist (so requires_auth_for_url returns true) but have no header_value
    // (so headers_for_url returns empty).
    let auth = Arc::new(
        AuthManager::from_config(&AuthConfig {
            auth_type: "bearer-token".to_string(),
            token_env: Some("DEFINITELY_NOT_SET_12345".to_string()),
            token_file: None,
            domain: Some("api.github.com".to_string()),
            header: None,
            scopes: None,
        })
        .unwrap(),
    );

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
    }));
    let executor = HttpExecutor::new(auth, capability).unwrap();

    let req = wit_types::HttpRequest {
        method: "GET".to_string(),
        url: "https://api.github.com/repos".to_string(),
        headers: Vec::new(),
        body: None,
    };
    match executor.fetch(&req).await {
        wit_types::CalloutResult::CalloutError(wit_types::CalloutError {
            kind: wit_types::ErrorKind::Denied,
            retryable: false,
            ..
        }) => {},
        other => panic!("expected denied error, got {other:?}"),
    }
}
