//! Human-readable formatting for provider capability manifest entries.

use omnifs_provider::{CapabilityEntry, PreopenMode, PreopenedPath};

pub(crate) fn capability_label(entry: &CapabilityEntry) -> &'static str {
    match entry {
        CapabilityEntry::Domain { .. } => "Network domains",
        CapabilityEntry::GitRepo { .. } => "Git remotes",
        CapabilityEntry::UnixSocket { .. } => "Unix sockets",
        CapabilityEntry::PreopenedPath { .. } => "Filesystem preopens",
        CapabilityEntry::MemoryMb { .. } => "Memory limit",
        CapabilityEntry::FetchBlobBytes { .. } => "Fetch body limit",
        CapabilityEntry::ReadBlobBytes { .. } => "Blob read limit",
    }
}

pub(crate) fn capability_value(entry: &CapabilityEntry) -> String {
    let value = match entry {
        CapabilityEntry::Domain { value, .. }
        | CapabilityEntry::GitRepo { value, .. }
        | CapabilityEntry::UnixSocket { value, .. } => value.clone(),
        CapabilityEntry::PreopenedPath { value, .. } => {
            preopen_summary(std::slice::from_ref(value))
        },
        CapabilityEntry::MemoryMb { value, .. } => format!("{value} MiB"),
        CapabilityEntry::FetchBlobBytes { value, .. }
        | CapabilityEntry::ReadBlobBytes { value, .. } => value.to_string(),
    };
    if entry.is_dynamic() {
        format!("{value} (config-dependent)")
    } else {
        value
    }
}

fn preopen_summary(entries: &[PreopenedPath]) -> String {
    entries
        .iter()
        .map(|entry| {
            let mode = match entry.mode {
                PreopenMode::Ro => "ro",
                PreopenMode::Rw => "rw",
            };
            format!("{} -> {} ({mode})", entry.host, entry.guest)
        })
        .collect::<Vec<_>>()
        .join(", ")
}
