//! Style helpers for user-facing output.
//!
//! Each helper returns a `String` so callers can interpolate without
//! needing to import `owo_colors::OwoColorize`. Color emission respects
//! the `NO_COLOR` env var and TTY detection because the only callers
//! print through `anstream::println!` / `anstream::eprintln!`, which
//! strip ANSI sequences when the destination isn't a color-aware TTY.

use owo_colors::OwoColorize;

pub fn success(s: impl std::fmt::Display) -> String {
    format!("{}", s.green())
}

pub fn warn(s: impl std::fmt::Display) -> String {
    format!("{}", s.yellow())
}

pub fn error(s: impl std::fmt::Display) -> String {
    format!("{}", s.red())
}

pub fn dim(s: impl std::fmt::Display) -> String {
    format!("{}", s.dimmed())
}

pub fn accent(s: impl std::fmt::Display) -> String {
    format!("{}", s.cyan())
}

pub fn bold(s: impl std::fmt::Display) -> String {
    format!("{}", s.bold())
}

/// Bold text accented with a provider's CSS hex brand color (e.g. `#2496ED`).
///
/// Emits a truecolor escape; like the other helpers, the ANSI is stripped by
/// `anstream` when the destination isn't a color-aware TTY (`NO_COLOR`, pipe).
/// A malformed or unparsable hex falls back to plain bold rather than erroring.
pub fn hex_accent(s: impl std::fmt::Display, hex: &str) -> String {
    match parse_hex(hex) {
        Some((r, g, b)) => format!("{}", s.truecolor(r, g, b).bold()),
        None => bold(s),
    }
}

/// Parse a 3- or 6-digit CSS hex color into an RGB triple. Returns `None` for
/// anything else (4/8-digit alpha forms collapse to their RGB prefix).
fn parse_hex(hex: &str) -> Option<(u8, u8, u8)> {
    let digits = hex.strip_prefix('#')?;
    let expand = |s: &str| u8::from_str_radix(s, 16).ok();
    match digits.len() {
        3 | 4 => {
            let c = digits.as_bytes();
            let dup = |b: u8| expand(&format!("{}{}", b as char, b as char));
            Some((dup(c[0])?, dup(c[1])?, dup(c[2])?))
        },
        6 | 8 => Some((
            expand(&digits[0..2])?,
            expand(&digits[2..4])?,
            expand(&digits[4..6])?,
        )),
        _ => None,
    }
}
