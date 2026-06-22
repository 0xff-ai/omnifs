//! Integration tests for the omnifs home path layout used by the CLI.

// env variable names share common stems; allow similar names in this file.
#![allow(clippy::similar_names)]

mod common;

use common::with_env;
use omnifs_home::{
    CACHE_SUBDIR, CONFIG_FILE, CREDENTIALS_FILE, OMNIFS_HOME_ENV, ResolveError, WorkspaceLayout,
};

#[test]
fn under_root_builds_the_workspace_layout() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("workspace");

    let paths = WorkspaceLayout::under_root(&root);

    assert_eq!(paths.config_dir, root);
    assert_eq!(paths.config_file, paths.config_dir.join(CONFIG_FILE));
    assert_eq!(
        paths.credentials_file,
        paths.config_dir.join(CREDENTIALS_FILE)
    );
    assert_eq!(paths.cache_dir, paths.config_dir.join(CACHE_SUBDIR));
}

#[test]
fn resolve_requires_home_when_no_root_source_exists() {
    with_env(&[("HOME", None), (OMNIFS_HOME_ENV, None)], || {
        let error = WorkspaceLayout::resolve().unwrap_err();
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
            let paths = WorkspaceLayout::resolve().unwrap();
            assert_eq!(paths.config_dir, root);
            assert_eq!(paths.config_file, paths.config_dir.join(CONFIG_FILE));
            assert_eq!(paths.cache_dir, paths.config_dir.join(CACHE_SUBDIR));
        },
    );
}
