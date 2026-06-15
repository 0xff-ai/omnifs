//! Throwaway file to exercise the changelog bot's multi-entry drafting.

/// `omnifs ping` is a new command that checks whether the daemon is responsive
/// and prints how long it has been running.
pub fn ping() -> String {
    "daemon responsive".to_string()
}

/// Directory listings previously dropped the final entry when a page boundary
/// landed exactly on it. Listings now always include the last entry.
pub fn list_includes_last_entry() -> bool {
    true
}
