//! The one atomic-write helper for workspace-owned files.
//!
//! Every on-disk format owned by this crate (mount specs, config, launch
//! record, credentials) is meant to route writes through here so partial
//! writes can never corrupt workspace state.

use std::io::{self, Write};
use std::path::Path;

use atomic_write_file::OpenOptions as AtomicOpenOptions;

/// Write `bytes` to `path` atomically: stage into a temporary file in the
/// same directory, then rename over the destination. On unix the file is
/// created with `mode` permission bits (an existing file's mode is not
/// preserved).
#[cfg_attr(not(unix), allow(unused_variables))]
pub fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let mut options = AtomicOpenOptions::new();
    #[cfg(unix)]
    {
        use atomic_write_file::unix::OpenOptionsExt as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        options.preserve_mode(false).mode(mode);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.commit()
}

#[cfg(test)]
mod tests {
    use super::write_atomic;

    #[test]
    fn writes_bytes_and_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.json");
        write_atomic(&path, b"{}", 0o600).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"{}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn replaces_existing_content_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.json");
        write_atomic(&path, b"old", 0o600).unwrap();
        write_atomic(&path, b"new", 0o600).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
    }
}
