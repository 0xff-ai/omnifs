//! Format [`InspectorEvent`] records for terminal output.

use omnifs_api::events::{CacheKind, InspectorEvent, InspectorRecord, TraceId};

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
        format_event(record.trace_id, &record.event)
    )
}

// One exhaustive variant ladder over `InspectorEvent`. Splitting it into
// per-variant helpers obscures the wire-vs-display correspondence
// without saving a meaningful number of lines, so the lint is allowed.
#[allow(clippy::too_many_lines)]
fn format_event(trace_id: TraceId, event: &InspectorEvent) -> String {
    match event {
        InspectorEvent::FuseStart { op, mount, path } => {
            format!("fuse.start #{trace_id} {op} {}", mount_path(mount, path))
        },
        InspectorEvent::FuseEnd { op, end } => format!(
            "fuse.end   #{trace_id} {op} {} {}µs",
            end.result.outcome, end.elapsed_us
        ),
        InspectorEvent::ProviderStart {
            operation_id,
            mount,
            provider,
            method,
            path,
        } => format!(
            "provider.start #{trace_id} op={operation_id} {mount}/{provider} {method} {path}"
        ),
        InspectorEvent::ProviderEnd { operation_id, end } => format!(
            "provider.end   #{trace_id} op={operation_id} {} {}µs",
            end.result.outcome, end.elapsed_us
        ),
        InspectorEvent::CalloutStart {
            operation_id,
            callout_index,
            kind,
            summary,
        } => format!(
            "callout.start #{trace_id} op={operation_id} idx={callout_index} {kind} {summary}"
        ),
        InspectorEvent::CalloutEnd {
            operation_id,
            callout_index,
            end,
        } => format!(
            "callout.end   #{trace_id} op={operation_id} idx={callout_index} {} {}µs",
            end.result.outcome, end.elapsed_us
        ),
        InspectorEvent::SubtreeStart {
            operation_id,
            tree_ref,
        } => format!("subtree.start #{trace_id} op={operation_id} {tree_ref}"),
        InspectorEvent::SubtreeEnd {
            operation_id,
            tree_ref,
            end,
        } => format!(
            "subtree.end   #{trace_id} op={operation_id} {tree_ref} {} {}µs",
            end.result.outcome, end.elapsed_us
        ),
        InspectorEvent::CloneStart {
            operation_id,
            cache_key,
            remote,
        } => format!("clone.start #{trace_id} op={operation_id} {cache_key} {remote}"),
        InspectorEvent::CloneEnd {
            operation_id,
            cache_key,
            end,
        } => format!(
            "clone.end   #{trace_id} op={operation_id} {cache_key} {} {}µs",
            end.result.outcome, end.elapsed_us
        ),
        InspectorEvent::CacheEvent {
            operation_id,
            mount,
            path,
            kind,
            elapsed_us,
        } => format_cache(trace_id, *operation_id, mount, path, *kind, *elapsed_us),
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

pub fn shorten_path(path: &str, max: usize) -> String {
    if max <= 3 {
        return "…".to_string();
    }
    if path.chars().count() <= max {
        return path.to_string();
    }
    let trimmed = path.trim_matches('/');
    let parts: Vec<&str> = trimmed.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() >= 3 {
        let head = parts[0];
        let tail = parts[parts.len() - 1];
        let candidate = format!("{head}/…/{tail}");
        if candidate.chars().count() <= max {
            return candidate;
        }
    }
    if parts.len() == 2 {
        let head: String = parts[0].chars().take(8).collect();
        let candidate = format!("{head}…/{}", parts[1]);
        if candidate.chars().count() <= max {
            return candidate;
        }
    }
    let suffix_start = path
        .char_indices()
        .rev()
        .nth(max - 2)
        .map_or(0, |(index, _)| index);
    format!("…{}", &path[suffix_start..])
}

// Microsecond elapsed times are presented to users with one decimal of
// precision; the f64 cast cannot lose visible precision at this scale.
#[allow(clippy::cast_precision_loss)]
pub fn format_latency_us(us: u64) -> String {
    if us >= 1_000_000 {
        format!("{:.1}s", us as f64 / 1_000_000.0)
    } else if us >= 1_000 {
        format!("{:.1}ms", us as f64 / 1_000.0)
    } else {
        format!("{us}µs")
    }
}

pub fn compact_mode(cols: u16, rows: u16) -> bool {
    cols < 80 || rows < 24
}

#[cfg(test)]
mod tests {
    use super::shorten_path;

    #[test]
    fn non_ascii_path_respects_character_width() {
        assert_eq!(shorten_path("éééé", 4), "éééé");
        assert_eq!(shorten_path("日本/中間/文件", 7), "日本/…/文件");
        assert_eq!(
            shorten_path("日日日日日日日日日日/leaf", 14),
            "日日日日日日日日…/leaf"
        );
    }
}
