//! Integration tests for the workspace broker's home resolution.

#![allow(clippy::similar_names)]

mod common;

use common::with_env;
use omnifs_workspace::{OMNIFS_HOME_ENV, ResolveError, Workspace};

#[test]
fn workspace_under_root_owns_component_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("workspace");
    let workspace = Workspace::under_root(&root);

    assert_eq!(workspace.daemon().record_file(), root.join("daemon.json"));
    assert_eq!(
        workspace.frontend().local_attach_socket(),
        root.join("frontends/local.sock")
    );
    assert_eq!(
        workspace.frontend().default_host_location(),
        root.join("omnifs")
    );
}

#[test]
fn workspace_resolve_requires_home_or_omnifs_home() {
    with_env(&[("HOME", None), (OMNIFS_HOME_ENV, None)], || {
        let Err(error) = Workspace::resolve() else {
            panic!("workspace unexpectedly resolved");
        };
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
            let workspace = Workspace::resolve().unwrap();
            assert_eq!(workspace.identity().output_home(), root);
        },
    );
}
