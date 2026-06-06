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
