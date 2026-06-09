//! Shared helpers for integration tests.

// env mutation helpers use unsafe set_var/remove_var (Rust 2024), allowed here
// because we hold ENV_LOCK across every mutation/restore pair.
#![allow(unsafe_code)]

use std::sync::Mutex;

// Guard for env-mutating tests: env is process-global, so all tests that touch
// it must hold this lock.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Set environment variables for the duration of `f`, then restore previous values.
pub fn with_env<F: FnOnce()>(vars: &[(&str, Option<&str>)], f: F) {
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let saved: Vec<(&str, Option<String>)> = vars
        .iter()
        .map(|(key, _)| (*key, std::env::var(*key).ok()))
        .collect();

    // SAFETY: ENV_LOCK is held for the entire duration of this call.
    // No other thread mutates the environment concurrently.
    for (key, value) in vars {
        match value {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
    }

    f();

    // SAFETY: ENV_LOCK is still held; restoring the saved values is subject
    // to the same serialization guarantee as the writes above.
    for (key, original) in &saved {
        match original {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
    }
}
