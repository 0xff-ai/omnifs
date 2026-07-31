//! Integration tests for profile bootstrap-path resolution.

#![allow(clippy::similar_names)]

mod common;

use common::with_env;
use omnifs_bootstrap::{OMNIFS_HOME_ENV, Profile, ResolveError};

#[test]
fn endpoint_under_root_owns_only_bootstrap_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("profile");
    let endpoint = Profile::under_root(&root);

    assert_eq!(endpoint.root(), root);
    assert_eq!(endpoint.control_socket(), root.join("control.sock"));
    assert_eq!(endpoint.process_identity_path(), root.join("process.json"));
}

#[test]
fn endpoint_resolve_requires_home_or_omnifs_home() {
    with_env(&[("HOME", None), (OMNIFS_HOME_ENV, None)], || {
        let Err(error) = Profile::resolve() else {
            panic!("endpoint unexpectedly resolved");
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
            let endpoint = Profile::resolve().unwrap();
            assert_eq!(endpoint.root(), root);
        },
    );
}
