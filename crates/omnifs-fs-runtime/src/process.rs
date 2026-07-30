use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context as _, Result};

type PollFuture<'a, T> = Pin<Box<dyn Future<Output = anyhow::Result<Option<T>>> + 'a>>;

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

pub(crate) fn new_instance_id(what: &str) -> anyhow::Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| anyhow::anyhow!("generate {what} instance id: {error}"))?;
    Ok(hex::encode(bytes))
}

#[cfg(test)]
pub(crate) fn is_alive(pid: u32) -> bool {
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}
