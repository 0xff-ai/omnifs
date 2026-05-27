//! Layout helpers: path shortening, timing labels.

pub fn shorten_path(path: &str, max: usize) -> String {
    if max <= 3 {
        return "…".to_string();
    }
    if path.len() <= max {
        return path.to_string();
    }
    let trimmed = path.trim_matches('/');
    let parts: Vec<&str> = trimmed.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() >= 3 {
        let head = parts[0];
        let tail = parts[parts.len() - 1];
        let candidate = format!("{head}/…/{tail}");
        if candidate.len() <= max {
            return candidate;
        }
    }
    if parts.len() == 2 {
        let candidate = format!("{}…/{}", &parts[0][..parts[0].len().min(8)], parts[1]);
        if candidate.len() <= max {
            return candidate;
        }
    }
    format!("…{}", &path[path.len().saturating_sub(max - 1)..])
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
