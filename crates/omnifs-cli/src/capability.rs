//! Human-readable formatting for provider capability manifest entries.

use omnifs_caps::{AccessNeed, LimitDeclarations, PreopenMode, PreopenedPath};

pub(crate) fn capability_label(entry: &AccessNeed) -> &'static str {
    match entry {
        AccessNeed::Domain { .. } => "Network domains",
        AccessNeed::GitRepo { .. } => "Git remotes",
        AccessNeed::UnixSocket { .. } => "Unix sockets",
        AccessNeed::PreopenedPath { .. } => "Filesystem preopens",
    }
}

pub(crate) fn capability_value(entry: &AccessNeed) -> String {
    let value = match entry {
        AccessNeed::Domain { value, .. }
        | AccessNeed::GitRepo { value, .. }
        | AccessNeed::UnixSocket { value, .. } => value.clone(),
        AccessNeed::PreopenedPath { value, .. } => preopen_summary(value),
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

pub(crate) struct LimitLine<'a> {
    pub(crate) label: &'static str,
    pub(crate) value: String,
    pub(crate) why: &'a str,
}

pub(crate) fn limit_lines(limits: &LimitDeclarations) -> Vec<LimitLine<'_>> {
    let mut lines = Vec::new();
    if let Some(limit) = &limits.max_memory_mb {
        lines.push(LimitLine {
            label: "Memory limit",
            value: format!("{} MiB", limit.value),
            why: &limit.why,
        });
    }
    if let Some(limit) = &limits.max_fetch_blob_bytes {
        lines.push(LimitLine {
            label: "Fetch body limit",
            value: limit.value.to_string(),
            why: &limit.why,
        });
    }
    if let Some(limit) = &limits.max_read_blob_bytes {
        lines.push(LimitLine {
            label: "Blob read limit",
            value: limit.value.to_string(),
            why: &limit.why,
        });
    }
    lines
}
