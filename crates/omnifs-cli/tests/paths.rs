//! Integration tests for `omnifs_cli::paths`.

// env variable names share common stems; allow similar names in this file.
#![allow(clippy::similar_names)]

mod common;

use common::with_env;
use omnifs_cli::paths::{PathOverrides, Paths};

#[test]
fn resolve_flag_overrides_win_over_env() {
    let tmp = tempfile::tempdir().unwrap();
    let env_config = tmp.path().join("env_config");
    let flag_config = tmp.path().join("flag_config");

    with_env(
        &[
            ("OMNIFS_CONFIG_DIR", Some(env_config.to_str().unwrap())),
            ("OMNIFS_HOME", None),
            ("XDG_CONFIG_HOME", None),
            ("OMNIFS_CACHE_DIR", None),
            ("OMNIFS_PROVIDERS_DIR", None),
        ],
        || {
            let paths = Paths::resolve(PathOverrides {
                config_dir: Some(flag_config.clone()),
                ..Default::default()
            });
            // Flag override wins over env var.
            assert_eq!(paths.config_dir, flag_config);
            assert_eq!(paths.config_file, paths.config_dir.join("config.toml"));
            assert_eq!(
                paths.credentials_file,
                paths.config_dir.join("credentials.json")
            );
        },
    );
}
