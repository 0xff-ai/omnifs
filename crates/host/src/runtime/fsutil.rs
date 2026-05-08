//! Small filesystem helpers shared across the runtime.

use std::path::Path;

/// Write `bytes` to `path` atomically: write to `<path>.tmp`, then
/// rename. Callers that don't care about partial-failure visibility
/// (e.g. opportunistic sidecar files) can ignore the error; callers
/// that need durability should propagate it.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
