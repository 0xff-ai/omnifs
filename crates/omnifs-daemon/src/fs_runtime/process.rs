use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context as _, Result};

type PollFuture<'a, T> = Pin<Box<dyn Future<Output = anyhow::Result<Option<T>>> + Send + 'a>>;

pub(crate) async fn poll_until<T, Fut>(
    timeout: Duration,
    interval: Duration,
    mut check: impl FnMut() -> Fut + Send,
) -> anyhow::Result<Option<T>>
where
    T: Send,
    Fut: Future<Output = anyhow::Result<Option<T>>> + Send,
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

pub(crate) async fn poll_until_mut<S, T>(
    timeout: Duration,
    interval: Duration,
    state: &mut S,
    mut check: impl for<'a> FnMut(&'a mut S) -> PollFuture<'a, T> + Send,
) -> anyhow::Result<Option<T>>
where
    S: Send,
    T: Send,
{
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

#[derive(Clone, Copy)]
pub(crate) enum LogMode {
    Append,
    TruncateRestricted0600,
}

pub(crate) fn configure_detached_child(
    command: &mut Command,
    log_path: &Path,
    mode: LogMode,
) -> Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).write(true);
    match mode {
        LogMode::Append => {
            options.append(true);
        },
        LogMode::TruncateRestricted0600 => {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.truncate(true).mode(0o600);
        },
    }
    let log = options
        .open(log_path)
        .with_context(|| format!("open log {}", log_path.display()))?;
    let stderr = log
        .try_clone()
        .with_context(|| format!("clone log {}", log_path.display()))?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    Ok(())
}

pub(crate) fn is_alive(pid: u32) -> bool {
    if pid == 0 || pid == u32::MAX {
        return false;
    }
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Keep ownership of a launched runtime process until the kernel reports its
/// exit, then reap it. Dropping `Child` after readiness would leave an exited
/// daemon child as a zombie until the daemon itself exits.
pub(crate) fn reap_managed_child(mut child: Child) {
    tokio::spawn(async move {
        loop {
            match child.try_wait() {
                Ok(Some(_)) | Err(_) => return,
                Ok(None) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn managed_child_is_reaped_after_exit() {
        let child = Command::new("true").spawn().unwrap();
        let pid = child.id();
        reap_managed_child(child);

        assert!(
            poll_until(
                Duration::from_secs(2),
                Duration::from_millis(20),
                || async { Ok((!is_alive(pid)).then_some(())) },
            )
            .await
            .unwrap()
            .is_some(),
            "managed child {pid} remained visible after exit"
        );
    }
}
