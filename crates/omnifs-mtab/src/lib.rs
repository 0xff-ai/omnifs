//! Shared mount-table mechanics for omnifs frontends.

#[cfg(any(target_os = "linux", test))]
pub mod proc_mounts;
pub mod state;
pub mod unmount;

pub use state::{NfsMountState, StateError, StateFile};
pub use unmount::{Platform, UnmountCommand, UnmountError};
