//! Shared mount-table mechanics for omnifs filesystems.

#[cfg(any(target_os = "linux", test))]
pub mod proc_mounts;
pub mod runner;
pub mod state;
pub mod unmount;

pub use runner::{
    RunnerClaim, RunnerRecord, RunnerRecordError, RunnerRecordFile, process_group_exists,
};
pub use state::{MountKind, MountState, StateError, StateFile};
pub use unmount::{Platform, UnmountCommand, UnmountError};
