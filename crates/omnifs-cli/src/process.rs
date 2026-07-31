//! Unix process probes shared by detached runtime owners, and the shared
//! deadline-poll loop every one of them needs while waiting for a runner,
//! helper, or daemon to reach some observable state.

use std::future::Future;
use std::pin::Pin;
use std::process::{Command, Stdio};
use std::time::Duration;

/// A boxed, borrowed-state future for [`poll_until_mut`]: unlike a plain
/// generic `Fut` type parameter, a trait object lets the returned future's
/// lifetime vary with each call's `&mut S` argument (a higher-ranked bound
/// a fixed `Fut` type cannot express).
type PollFuture<'a, T> = Pin<Box<dyn Future<Output = anyhow::Result<Option<T>>> + 'a>>;

/// Poll `check` at `interval` until it returns `Ok(Some(value))` (ready), an
/// `Err` (an immediate hard failure, not subject to the deadline), or return
/// `Ok(None)` once `timeout` elapses without either. `check` returning
/// `Ok(None)` means "not ready yet, keep polling."
///
/// A caller whose own body cannot fail folds any hard-abort condition into
/// `Ok(Some(value))` instead and ignores the (then-unreachable) `Err` case,
/// rather than inventing a placeholder error type.
#[allow(
    dead_code,
    reason = "Plan 006 removes the last superseded client runtime caller"
)]
pub(crate) async fn poll_until<T, Fut>(
    timeout: Duration,
    interval: Duration,
    mut check: impl FnMut() -> Fut,
) -> anyhow::Result<Option<T>>
where
    Fut: Future<Output = anyhow::Result<Option<T>>>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(value) = check().await? {
            return Ok(Some(value));
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(None);
        }
        tokio::time::sleep(interval).await;
    }
}

/// The variant of [`poll_until`] for a `check` that needs `&mut` access to
/// state across iterations (for example, polling a child process through
/// `try_wait`, or recording the last observed state for a timeout message).
/// `state` is threaded through as an explicit argument, rather than let
/// `check` capture it, because an `FnMut` closure cannot itself return a
/// future that borrows its own captured environment; a fresh reborrow of an
/// argument does not have that restriction.
pub(crate) async fn poll_until_mut<S, T>(
    timeout: Duration,
    interval: Duration,
    state: &mut S,
    mut check: impl for<'a> FnMut(&'a mut S) -> PollFuture<'a, T>,
) -> anyhow::Result<Option<T>> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(value) = check(state).await? {
            return Ok(Some(value));
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(None);
        }
        tokio::time::sleep(interval).await;
    }
}

/// The synchronous twin of [`poll_until`] for the one caller
/// (`commands/mount/detect.rs`) that runs outside the async runtime. `check`
/// returning `Some(value)` means ready (a caller folds any hard-abort
/// condition into this rather than a separate error channel); `None` means
/// keep polling.
pub(crate) fn poll_until_sync<T>(
    timeout: Duration,
    interval: Duration,
    mut check: impl FnMut() -> Option<T>,
) -> Option<T> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(value) = check() {
            return Some(value);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(interval);
    }
}

/// How the omnifs process is running, which sets its default tracing level.
#[derive(Clone, Copy)]
pub(crate) enum ProcessRole {
    /// A foreground CLI invocation: stays quiet so ordinary commands are not
    /// noisy.
    Cli,
    /// A background daemon the CLI spawned: defaults louder so its startup
    /// diagnostics are captured in daemon.log rather than hidden.
    Daemon,
}

impl ProcessRole {
    /// The default `RUST_LOG` level for this process role.
    pub(crate) const fn default_log_level(self) -> &'static str {
        match self {
            Self::Cli => "warn",
            Self::Daemon => "info",
        }
    }
}

/// Whether `kill -0` reports `pid` as a live process.
pub(crate) fn is_alive(pid: u32) -> bool {
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    use super::is_alive;

    #[test]
    fn distinguishes_current_and_exited_processes() {
        assert!(is_alive(std::process::id()));

        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(!is_alive(pid));
    }
}
