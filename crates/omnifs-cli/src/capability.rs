//! Human-readable formatting for provider capability manifest entries.
//!
//! This module is the single owner of capability prose. `omnifs mount add`
//! renders a manifest's needs and limits through these helpers, so the wording
//! stays consistent.

use omnifs_caps::{AccessNeed, LimitDeclarations, PreopenMode, PreopenedPath};
use omnifs_workspace::provider::ProviderManifest;

/// Maximum visible width of a compact needs line.
const COMPACT_NEEDS_WIDTH: usize = 72;

/// Needs grouped by identical justification, in declaration order:
/// `[(joined values, why)]`.
fn need_groups(manifest: &ProviderManifest) -> Vec<(String, String)> {
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    for entry in &manifest.capabilities {
        let why = entry.why().to_string();
        let value = need_display(entry);
        match groups.iter_mut().find(|(seen, _)| *seen == why) {
            Some((_, values)) => values.push(value),
            None => groups.push((why, vec![value])),
        }
    }
    groups
        .into_iter()
        .map(|(why, values)| (values.join(", "), why))
        .collect()
}

/// Compact one-line needs summary, e.g.
/// `needs api.github.com (API calls), git@github.com:* (clones)`.
///
/// Values that share a justification are grouped; at most two groups render
/// before a trailing `…`. Capped at [`COMPACT_NEEDS_WIDTH`] visible columns by
/// truncating the parenthetical justifications.
pub(crate) fn compact_needs(manifest: &ProviderManifest) -> Option<String> {
    if manifest.capabilities.is_empty() {
        return None;
    }
    let groups = need_groups(manifest);
    let shown = &groups[..groups.len().min(2)];
    let overflow = groups.len() > 2;

    // Everything except the whys is fixed width; whatever budget remains is
    // split evenly across the parentheticals.
    let fixed = "needs ".chars().count()
        + shown
            .iter()
            .map(|(values, _)| values.chars().count() + " ()".chars().count())
            .sum::<usize>()
        + ", ".len() * shown.len().saturating_sub(1)
        + if overflow { ", …".chars().count() } else { 0 };
    let why_budget = COMPACT_NEEDS_WIDTH.saturating_sub(fixed) / shown.len().max(1);

    let mut rendered: Vec<String> = shown
        .iter()
        .map(|(values, why)| format!("{values} ({})", crate::ui::truncate(why, why_budget)))
        .collect();
    if overflow {
        rendered.push("…".to_string());
    }
    // Safety net for pathological value lists.
    Some(crate::ui::truncate(
        &format!("needs {}", rendered.join(", ")),
        COMPACT_NEEDS_WIDTH,
    ))
}

/// Compact one-line limits summary, e.g. `up to 256 MiB memory`.
pub(crate) fn compact_limits(manifest: &ProviderManifest) -> Option<String> {
    let lines = limit_lines(&manifest.limits);
    if lines.is_empty() {
        return None;
    }
    let parts: Vec<String> = lines
        .iter()
        .map(|line| format!("up to {} {}", line.value, limit_noun(line.label)))
        .collect();
    Some(parts.join(", "))
}

/// Short noun for a limit kind used in compact and detail rendering.
fn limit_noun(label: &str) -> &'static str {
    match label {
        "Fetch body limit" => "per fetch",
        "Blob read limit" => "per read",
        _ => "memory",
    }
}

/// User-facing rendering of one access need. A dynamic need's manifest value is
/// an internal placeholder, so it renders as plain language derived from the
/// need's kind instead.
fn need_display(entry: &AccessNeed) -> String {
    if entry.is_dynamic() {
        return match entry {
            AccessNeed::Domain { .. } => "domains set by this mount's config",
            AccessNeed::GitRepo { .. } => "git remotes set by this mount's config",
            AccessNeed::UnixSocket { .. } => "socket path set by this mount's config",
            AccessNeed::PreopenedPath { .. } => "file path set by this mount's config",
        }
        .to_string();
    }
    match entry {
        AccessNeed::Domain { value, .. }
        | AccessNeed::GitRepo { value, .. }
        | AccessNeed::UnixSocket { value, .. } => value.clone(),
        AccessNeed::PreopenedPath { value, .. } => preopen_summary(value),
    }
}

fn preopen_summary(entry: &PreopenedPath) -> String {
    let mode = match entry.mode {
        PreopenMode::Ro => "ro",
        PreopenMode::Rw => "rw",
    };
    format!("{} -> {} ({mode})", entry.host, entry.guest)
}

pub(crate) struct LimitLine {
    pub(crate) label: &'static str,
    pub(crate) value: String,
}

pub(crate) fn limit_lines(limits: &LimitDeclarations) -> Vec<LimitLine> {
    let mut lines = Vec::new();
    if let Some(limit) = &limits.max_memory_mb {
        lines.push(LimitLine {
            label: "Memory limit",
            value: format!("{} MiB", limit.value),
        });
    }
    if let Some(limit) = &limits.max_fetch_blob_bytes {
        lines.push(LimitLine {
            label: "Fetch body limit",
            value: limit.value.to_string(),
        });
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_caps::AccessNeed;

    fn manifest_with_needs(needs: Vec<AccessNeed>) -> ProviderManifest {
        ProviderManifest {
            id: "test".to_string(),
            display_name: "Test".to_string(),
            description: None,
            provider: "omnifs_provider_test.wasm".to_string(),
            default_mount: "test".to_string(),
            version: None,
            wit_package: None,
            sdk_version: None,
            refresh_interval_secs: 0,
            capabilities: needs,
            limits: LimitDeclarations::default(),
            auth: None,
            config: None,
        }
    }

    fn domain(value: &str, why: &str) -> AccessNeed {
        AccessNeed::Domain {
            value: value.to_string(),
            why: why.to_string(),
            dynamic: false,
        }
    }

    #[test]
    fn compact_needs_caps_at_72_columns() {
        let long_why = "Fetch arXiv API metadata and paper resources from arXiv-owned domains.";
        let manifest = manifest_with_needs(vec![
            domain("export.arxiv.org", long_why),
            domain("arxiv.org", long_why),
        ]);
        let line = compact_needs(&manifest).expect("needs line");
        assert!(
            line.chars().count() <= 72,
            "line is {} chars: {line:?}",
            line.chars().count()
        );
        assert!(line.starts_with("needs export.arxiv.org, arxiv.org ("));
        assert!(line.contains('…'), "truncated why must carry an ellipsis");
    }
}
