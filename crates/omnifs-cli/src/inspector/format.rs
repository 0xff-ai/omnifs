//! Formatting policies shared by the inspector UI and trace model.

use omnifs_api::events::CacheKind;

/// Map a wire `CacheKind` to the user-facing display label. The wire
/// schema distinguishes browse/file/blob tiers so a debugger can see
/// exactly which tier responded, but in the live UI that distinction
/// is noise; collapse it to `cache.hit` / `cache.miss` and keep the
/// non-hit/miss variants by their literal name. Shared by the
/// plain-mode formatter and the TUI's stage construction so both
/// surfaces use the same vocabulary.
pub(super) fn cache_event_label(kind: CacheKind) -> &'static str {
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

pub(super) fn shorten_path(path: &str, max: usize) -> String {
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
pub(super) fn format_latency_us(us: u64) -> String {
    if us >= 1_000_000 {
        format!("{:.1}s", us as f64 / 1_000_000.0)
    } else if us >= 1_000 {
        format!("{:.1}ms", us as f64 / 1_000.0)
    } else {
        format!("{us}µs")
    }
}

pub(super) fn compact_mode(cols: u16, rows: u16) -> bool {
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
