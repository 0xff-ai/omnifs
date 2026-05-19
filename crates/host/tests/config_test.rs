use omnifs_host::config::{InstanceConfig, PreopenMode};

#[test]
fn test_parse_minimal_config() {
    let json = r#"{
        "plugin": "test.wasm",
        "mount": "test"
    }"#;
    let config = InstanceConfig::parse(json).unwrap();
    assert_eq!(config.plugin, "test.wasm");
    assert_eq!(config.mount, "test");
    assert!(config.auth.is_empty());
    assert!(config.capabilities.is_none());
    assert!(config.config_raw.is_none());
}

#[test]
fn test_parse_full_config() {
    let json = r#"{
        "plugin": "github.wasm",
        "mount": "github",
        "auth": {
            "type": "bearer-token",
            "token_env": "GITHUB_TOKEN"
        },
        "capabilities": {
            "domains": ["api.github.com"],
            "max_memory_mb": 128
        },
        "config": {
            "issue_format": "markdown",
            "include_pr_diff": true
        }
    }"#;
    let config = InstanceConfig::parse(json).unwrap();
    assert_eq!(config.plugin, "github.wasm");
    assert_eq!(config.mount, "github");
    assert_eq!(config.auth.len(), 1);
    assert!(config.capabilities.is_some());
    assert!(config.config_raw.is_some());
}

#[test]
fn test_parse_missing_required_field() {
    let json = r#"{
        "mount": "test"
    }"#;
    let result = InstanceConfig::parse(json);
    assert!(result.is_err());
}

#[test]
fn test_auth_bearer_token_from_env() {
    let json = r#"{
        "plugin": "test.wasm",
        "mount": "test",
        "auth": {
            "type": "bearer-token",
            "token_env": "TEST_TOKEN"
        }
    }"#;
    let config = InstanceConfig::parse(json).unwrap();
    assert_eq!(config.auth.len(), 1);
    let auth = &config.auth[0];
    assert_eq!(auth.auth_type, "bearer-token");
    assert_eq!(auth.token_env.as_deref(), Some("TEST_TOKEN"));
    assert_eq!(auth.token_file, None);
}

#[test]
fn test_auth_bearer_token_from_file() {
    let json = r#"{
        "plugin": "test.wasm",
        "mount": "test",
        "auth": {
            "type": "bearer-token",
            "token_file": "/run/secrets/github_token"
        }
    }"#;
    let config = InstanceConfig::parse(json).unwrap();
    assert_eq!(config.auth.len(), 1);
    let auth = &config.auth[0];
    assert_eq!(auth.auth_type, "bearer-token");
    assert_eq!(auth.token_env, None);
    assert_eq!(
        auth.token_file.as_deref(),
        Some("/run/secrets/github_token")
    );
}

#[test]
fn test_parse_preopened_paths_defaults_to_ro() {
    let json = r#"{
        "plugin": "db.wasm",
        "mount": "db",
        "capabilities": {
            "preopened_paths": [
                { "host": "/data", "guest": "/data" }
            ]
        }
    }"#;
    let config = InstanceConfig::parse(json).unwrap();
    let preopens = config
        .capabilities
        .as_ref()
        .and_then(|c| c.preopened_paths.as_ref())
        .expect("preopened_paths present");
    assert_eq!(preopens.len(), 1);
    assert_eq!(preopens[0].host, "/data");
    assert_eq!(preopens[0].guest, "/data");
    assert_eq!(preopens[0].mode, PreopenMode::Ro);
}

#[test]
fn test_parse_preopened_paths_explicit_rw() {
    let json = r#"{
        "plugin": "db.wasm",
        "mount": "db",
        "capabilities": {
            "preopened_paths": [
                { "host": "/data", "guest": "/data", "mode": "rw" }
            ]
        }
    }"#;
    let config = InstanceConfig::parse(json).unwrap();
    let preopens = config
        .capabilities
        .as_ref()
        .and_then(|c| c.preopened_paths.as_ref())
        .expect("preopened_paths present");
    assert_eq!(preopens[0].mode, PreopenMode::Rw);
}
