//! Integration tests for the omnifs home path layout used by the CLI.

// env variable names share common stems; allow similar names in this file.
#![allow(clippy::similar_names)]

mod common;

use common::with_env;
use omnifs_workspace::daemon_record::{CONTROL_SOCKET_FILE, DAEMON_RECORD_FILE};
use omnifs_workspace::layout::{
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
    assert_eq!(
        paths.daemon_record_file(),
        paths.config_dir.join(DAEMON_RECORD_FILE)
    );
    assert_eq!(
        paths.control_socket(),
        paths.config_dir.join(CONTROL_SOCKET_FILE)
    );
    assert_eq!(paths.cache_dir, paths.config_dir.join(CACHE_SUBDIR));
}

#[test]
fn workspace_layout_resolve_requires_home_or_omnifs_home() {
    with_env(&[("HOME", None), (OMNIFS_HOME_ENV, None)], || {
        let error = WorkspaceLayout::resolve().unwrap_err();
        assert_eq!(error, ResolveError);
    });

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
        },
    );
}
