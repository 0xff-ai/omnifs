//! Shared identity guards used by every runtime backend and the driver
//! dispatch above them. Living beside neither kills the reverse coupling a
//! backend importing from `driver` would otherwise create.

use anyhow::{Result, ensure};
use omnifs_core::{FilesystemSpec, ResourceName};

pub(crate) fn ensure_record_matches(
    record_filesystem: &ResourceName,
    record_spec: &FilesystemSpec,
    expected_filesystem: &ResourceName,
    expected_spec: &FilesystemSpec,
) -> Result<()> {
    ensure!(
        record_filesystem == expected_filesystem && record_spec == expected_spec,
        "runner record does not match configured Filesystem `{expected_filesystem}`",
    );
    Ok(())
}

pub(crate) fn ensure_identity_unchanged<T: PartialEq>(
    current: Option<&T>,
    expected: &T,
    noun: &str,
) -> Result<()> {
    ensure!(
        current == Some(expected),
        "{noun} identity changed; refusing to touch its replacement"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use omnifs_core::{FilesystemProtocol, FilesystemRuntime};

    use super::*;

    #[test]
    fn record_and_runtime_identity_rechecks_fail_closed() {
        let name = ResourceName::new("main").unwrap();
        let recorded = FilesystemSpec::new(
            FilesystemProtocol::Nfs,
            FilesystemRuntime::Host,
            PathBuf::from("/tmp/recorded"),
            None,
            None,
        )
        .unwrap();
        let configured = FilesystemSpec::new(
            FilesystemProtocol::Nfs,
            FilesystemRuntime::Host,
            PathBuf::from("/tmp/configured"),
            None,
            None,
        )
        .unwrap();
        assert!(
            ensure_record_matches(&name, &recorded, &name, &configured)
                .unwrap_err()
                .to_string()
                .contains("runner record does not match")
        );
        assert!(
            ensure_identity_unchanged(Some(&2_u8), &1_u8, "runner")
                .unwrap_err()
                .to_string()
                .contains("refusing to touch its replacement")
        );
    }
}
