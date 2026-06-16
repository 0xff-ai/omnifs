use std::io;

#[derive(Debug, thiserror::Error)]
pub enum NfsFrontendError {
    #[error("mount command failed: {0}")]
    Mount(String),
    #[error("mount state failed: {0}")]
    State(String),
    #[error("unmount command failed: {0}")]
    Unmount(String),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}
