//! Format [`InspectorEvent`] records for terminal output.

use omnifs_inspector::{CacheKind, InspectorEvent, InspectorRecord};

/// Map a wire `CacheKind` to the user-facing display label. The wire
/// schema distinguishes browse/file/blob tiers so a debugger can see
/// exactly which tier responded, but in the live UI that distinction
/// is noise; collapse it to `cache.hit` / `cache.miss` and keep the
/// non-hit/miss variants by their literal name. Shared by the
/// plain-mode formatter and the TUI's stage construction so both
/// surfaces use the same vocabulary.
pub fn cache_event_label(kind: CacheKind) -> &'static str {
    use CacheKind::{
        BlobHit, BlobMiss, BrowseHit, BrowseMiss, FileHit, FileMiss, Invalidated, PreloadStored,
    };
    match kind {
        BrowseHit | FileHit | BlobHit => "cache.hit",
        BrowseMiss | FileMiss | BlobMiss => "cache.miss",
        PreloadStored => "cache.stored",
        Invalidated => "cache.invalidated",
    }
}

pub fn format_record(record: &InspectorRecord) -> String {
    format!(
        "[{} +{}µs] {}",
        record.ts,
        record.mono_us,
        format_event(&record.event)
    )
}

// One exhaustive variant ladder over `InspectorEvent`. Splitting it into
// per-variant helpers obscures the wire-vs-display correspondence
// without saving a meaningful number of lines, so the lint is allowed.
#[allow(clippy::too_many_lines)]
pub fn format_event(event: &InspectorEvent) -> String {
    match event {
        InspectorEvent::FuseStart {
            trace_id,
            op,
            mount,
            path,
        } => format!("fuse.start #{trace_id} {op} {}", mount_path(mount, path)),
        InspectorEvent::FuseEnd {
            trace_id,
            op,
            elapsed_us,
            result,
        } => format!(
            "fuse.end   #{trace_id} {op} {} {elapsed_us}µs",
            result.outcome
        ),
        InspectorEvent::ProviderStart {
            trace_id,
            operation_id,
            mount,
            provider,
            method,
            path,
        } => format!(
            "provider.start #{trace_id} op={operation_id} {mount}/{provider} {method} {path}"
        ),
        InspectorEvent::ProviderSuspend {
            trace_id,
            operation_id,
            callout_count,
        } => format!("provider.suspend #{trace_id} op={operation_id} callouts={callout_count}"),
        InspectorEvent::ProviderResume {
            trace_id,
            operation_id,
            round,
            result_count,
        } => format!(
            "provider.resume #{trace_id} op={operation_id} round={round} results={result_count}"
        ),
        InspectorEvent::ProviderEnd {
            trace_id,
            operation_id,
            elapsed_us,
            result,
        } => format!(
            "provider.end   #{trace_id} op={operation_id} {} {elapsed_us}µs",
            result.outcome
        ),
        InspectorEvent::CalloutStart {
            trace_id,
            operation_id,
            callout_index,
            kind,
            summary,
        } => format!(
            "callout.start #{trace_id} op={operation_id} idx={callout_index} {kind} {summary}"
        ),
        InspectorEvent::CalloutEnd {
            trace_id,
            operation_id,
            callout_index,
            elapsed_us,
            result,
        } => format!(
            "callout.end   #{trace_id} op={operation_id} idx={callout_index} {} {elapsed_us}µs",
            result.outcome
        ),
        InspectorEvent::SubtreeStart {
            trace_id,
            operation_id,
            tree_ref,
        } => format!("subtree.start #{trace_id} op={operation_id} {tree_ref}"),
        InspectorEvent::SubtreeEnd {
            trace_id,
            operation_id,
            tree_ref,
            elapsed_us,
            result,
        } => format!(
            "subtree.end   #{trace_id} op={operation_id} {tree_ref} {} {elapsed_us}µs",
            result.outcome
        ),
        InspectorEvent::CloneStart {
            trace_id,
            operation_id,
            cache_key,
            remote,
        } => format!("clone.start #{trace_id} op={operation_id} {cache_key} {remote}"),
        InspectorEvent::CloneEnd {
            trace_id,
            operation_id,
            cache_key,
            elapsed_us,
            result,
        } => format!(
            "clone.end   #{trace_id} op={operation_id} {cache_key} {} {elapsed_us}µs",
            result.outcome
        ),
        InspectorEvent::CacheEvent {
            trace_id,
            operation_id,
            mount,
            path,
            kind,
            elapsed_us,
        } => format_cache(*trace_id, *operation_id, mount, path, *kind, *elapsed_us),
    }
}

fn format_cache(
    trace_id: u64,
    operation_id: Option<u64>,
    mount: &str,
    path: &str,
    kind: CacheKind,
    elapsed_us: Option<u64>,
) -> String {
    let label = cache_event_label(kind);
    let op = operation_id.map_or_else(|| "host".to_string(), |id| format!("op={id}"));
    let timing = elapsed_us.map_or_else(String::new, |us| format!(" {us}µs"));
    format!(
        "{label} #{trace_id} {op} {}{timing}",
        mount_path(mount, path)
    )
}

fn mount_path(mount: &str, path: &str) -> String {
    if path.starts_with('/') {
        format!("{mount}{path}")
    } else {
        format!("{mount}/{path}")
    }
}
