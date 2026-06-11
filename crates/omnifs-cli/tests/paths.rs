//! Integration tests for `omnifs_cli::paths`.

// env variable names share common stems; allow similar names in this file.
#![allow(clippy::similar_names)]

mod common;

use common::with_env;
use omnifs_cli::paths::{PathOverrides, Paths, ResolveError};
use omnifs_home::{CACHE_SUBDIR, CONFIG_FILE, CREDENTIALS_FILE, OMNIFS_HOME_ENV};

#[test]
fn resolve_flag_overrides_win_over_env() {
    let tmp = tempfile::tempdir().unwrap();
    let env_home = tmp.path().join("env_home");
    let flag_config = tmp.path().join("flag_config");

    with_env(
        &[(OMNIFS_HOME_ENV, Some(env_home.to_str().unwrap()))],
        || {
            let paths = Paths::resolve(PathOverrides {
                config_dir: Some(flag_config.clone()),
                ..Default::default()
            })
            .unwrap();
            // Flag override wins over env var.
            assert_eq!(paths.config_dir, flag_config);
            assert_eq!(paths.config_file, paths.config_dir.join(CONFIG_FILE));
            assert_eq!(
                paths.credentials_file,
                paths.config_dir.join(CREDENTIALS_FILE)
            );
        },
    );
}

#[test]
fn resolve_requires_home_when_no_root_source_exists() {
    with_env(&[("HOME", None), (OMNIFS_HOME_ENV, None)], || {
        let error = Paths::resolve(PathOverrides::default()).unwrap_err();
        assert_eq!(error, ResolveError);
    });
}

#[test]
fn resolve_uses_omnifs_home_without_home() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("omnifs");

    with_env(
        &[
            ("HOME", None),
            (OMNIFS_HOME_ENV, Some(root.to_str().unwrap())),
        ],
        || {
            let paths = Paths::resolve(PathOverrides::default()).unwrap();
            assert_eq!(paths.config_dir, root);
            assert_eq!(paths.config_file, paths.config_dir.join(CONFIG_FILE));
            assert_eq!(paths.cache_dir, paths.config_dir.join(CACHE_SUBDIR));
        },
    );
}
