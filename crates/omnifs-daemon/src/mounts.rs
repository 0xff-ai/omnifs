//! Daemon-side mount lifecycle errors.

use omnifs_host::registry::RegistryError;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Registry(#[from] RegistryError),
    #[error("mount lifecycle task failed")]
    TaskFailed,
}
