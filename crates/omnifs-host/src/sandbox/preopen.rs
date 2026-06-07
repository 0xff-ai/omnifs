//! Helpers for constructing narrow WASI preopens.

use std::path::Path;

const STAGED_BLOB_FILE: &str = "blob.dat";

/// Failure while staging a host file for a sandbox preopen.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PreopenError {
    /// Staging requires the source file to live under a parent directory.
    #[error("blob has no parent: {0}")]
    MissingParent(String),
    /// Filesystem operation failed while preparing the staged input.
    #[error("{0}")]
    Io(String),
}

/// Per-invocation staging directory for a single blob input.
///
/// The sandbox receives this directory as a read-only preopen and sees
/// only `blob.dat` inside it. The temp directory is removed
/// when this value is dropped.
pub(crate) struct StagedBlob {
    dir: tempfile::TempDir,
}

impl StagedBlob {
    /// Stage `blob_path` next to the source file and expose it as
    /// `blob.dat`.
    ///
    /// A hardlink is preferred so large cached blobs are not copied on
    /// every tool invocation. If the cache location crosses devices,
    /// the helper falls back to copying.
    pub(crate) fn stage(blob_path: &Path) -> Result<Self, PreopenError> {
        let parent = blob_path
            .parent()
            .ok_or_else(|| PreopenError::MissingParent(blob_path.display().to_string()))?;
        let dir = tempfile::Builder::new()
            .prefix("omnifs-sandbox-")
            .tempdir_in(parent)
            .map_err(|e| PreopenError::Io(format!("tempdir: {e}")))?;
        let target = dir.path().join(STAGED_BLOB_FILE);
        if let Err(e) = std::fs::hard_link(blob_path, &target) {
            if matches!(e.kind(), std::io::ErrorKind::CrossesDevices)
                || e.raw_os_error() == Some(libc::EXDEV)
            {
                std::fs::copy(blob_path, &target)
                    .map_err(|e2| PreopenError::Io(format!("copy blob to scratch: {e2}")))?;
            } else {
                return Err(PreopenError::Io(format!("hard_link blob to scratch: {e}")));
            }
        }
        Ok(Self { dir })
    }

    /// Directory to preopen into the sandbox.
    pub(crate) fn dir(&self) -> &Path {
        self.dir.path()
    }
}
