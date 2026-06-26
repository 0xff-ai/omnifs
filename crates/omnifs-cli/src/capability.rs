//! Human-readable formatting for provider capability manifest entries.

use omnifs_caps::{Need, PreopenMode, PreopenedPath};

pub(crate) fn capability_label(entry: &Need) -> &'static str {
    match entry {
        Need::Domain { .. } => "Network domains",
        Need::GitRepo { .. } => "Git remotes",
        Need::UnixSocket { .. } => "Unix sockets",
        Need::PreopenedPath { .. } => "Filesystem preopens",
        Need::MemoryMb { .. } => "Memory limit",
        Need::FetchBlobBytes { .. } => "Fetch body limit",
        Need::ReadBlobBytes { .. } => "Blob read limit",
    }
}

pub(crate) fn capability_value(entry: &Need) -> String {
    let value = match entry {
        Need::Domain { value, .. }
        | Need::GitRepo { value, .. }
        | Need::UnixSocket { value, .. } => value.clone(),
        Need::PreopenedPath { value, .. } => preopen_summary(value),
        Need::MemoryMb { value, .. } => format!("{value} MiB"),
        Need::FetchBlobBytes { value, .. } | Need::ReadBlobBytes { value, .. } => value.to_string(),
    };
    if entry.is_dynamic() {
        format!("{value} (config-dependent)")
    } else {
        value
    }
}

fn preopen_summary(entry: &PreopenedPath) -> String {
    let mode = match entry.mode {
        PreopenMode::Ro => "ro",
        PreopenMode::Rw => "rw",
    };
    format!("{} -> {} ({mode})", entry.host, entry.guest)
}
