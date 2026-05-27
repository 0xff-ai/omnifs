//! `omnifs inspect` — live JSONL inspector TUI.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use anyhow::Context;
use clap::Args;
use omnifs_inspector::parse_record_line;

use crate::container_name::ContainerName;
use crate::inspector::{
    ConnectionMode, SourceKind, default_inspector_addr, format_record, run_plain, run_tui,
};
use crate::paths::{PathOverrides, Paths};
use crate::session::{self, ENV_CONTAINER_NAME};

#[derive(Args, Debug, Clone, Default)]
pub struct InspectArgs {
    /// Container name.
    ///
    /// Defaults to `OMNIFS_CONTAINER_NAME`, then configured session name, then
    /// `omnifs`.
    #[arg(long)]
    pub container_name: Option<String>,

    /// Replay a captured JSONL file instead of attaching live.
    #[arg(long, value_name = "FILE")]
    pub replay: Option<PathBuf>,

    /// While live-attaching, also append the stream to this host path.
    #[arg(long, value_name = "FILE")]
    pub record: Option<PathBuf>,

    /// Print raw JSONL instead of the ratatui canvas.
    #[arg(long)]
    pub plain: bool,
}

impl InspectArgs {
    pub async fn run(self) -> anyhow::Result<()> {
        if self.plain {
            return self.run_plain().await;
        }

        let (mode, source, container) = if let Some(path) = self.replay.clone() {
            (
                ConnectionMode::Replay,
                SourceKind::Replay(path),
                "replay".to_string(),
            )
        } else {
            let container = self.resolve_container()?;
            let addr = default_inspector_addr();
            let label = container.as_str().to_string();
            (
                ConnectionMode::Inspector,
                SourceKind::Socket {
                    addr,
                    record: self.record.clone(),
                },
                label,
            )
        };

        tokio::task::spawn_blocking(move || run_tui(mode, container, source))
            .await
            .context("inspector TUI task")??;
        Ok(())
    }

    async fn run_plain(self) -> anyhow::Result<()> {
        if let Some(path) = self.replay {
            return run_plain(SourceKind::Replay(path));
        }
        let _container = self.resolve_container()?;
        let addr = default_inspector_addr();
        let record = self.record.clone();
        tokio::task::spawn_blocking(move || socket_plain_attach(addr, record.as_deref()))
            .await
            .context("inspector plain task")?
    }

    fn resolve_container(&self) -> anyhow::Result<ContainerName> {
        let (_paths, config) = Paths::resolve_with_config(PathOverrides::default())?;
        let name = self
            .container_name
            .clone()
            .or_else(|| session::env_string(ENV_CONTAINER_NAME))
            .or(config.container_name)
            .unwrap_or_else(|| session::CONTAINER_NAME.to_string());
        ContainerName::new(name)
    }
}

/// Plain-mode driver: connect to the TCP socket, forward each line
/// to stdout (formatted on parse, raw on parse-failure), and
/// optionally append to a host-side record file.
fn socket_plain_attach(
    addr: SocketAddr,
    record_path: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let mut record = record_path.map(open_record_file).transpose()?;
    // Reconnect loop: lets the user start `omnifs inspect` before
    // `omnifs dev` finishes binding the socket.
    loop {
        let Ok(stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(500)) else {
            thread::sleep(Duration::from_millis(250));
            continue;
        };
        let reader = BufReader::new(stream);
        for line in reader.lines() {
            let line = line.context("read inspector stream")?;
            emit_plain_line(&line, record.as_mut())?;
        }
        thread::sleep(Duration::from_millis(500));
    }
}

fn open_record_file(path: &std::path::Path) -> anyhow::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open record file `{}`", path.display()))
}

fn emit_plain_line(line: &str, mut record: Option<&mut std::fs::File>) -> std::io::Result<()> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    match parse_record_line(trimmed) {
        Ok(record_parsed) => anstream::println!("{}", format_record(&record_parsed)),
        Err(_) => anstream::println!("{trimmed}"),
    }
    if let Some(file) = record.as_mut() {
        file.write_all(trimmed.as_bytes())?;
        file.write_all(b"\n")?;
        file.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_inspector::{InspectorEvent, InspectorRecord};
    use std::fs;
    use std::io::{Read, Seek};
    use tempfile::NamedTempFile;

    #[test]
    fn replay_plain_reads_jsonl_file() {
        let file = NamedTempFile::new().expect("tempfile");
        let record = InspectorRecord::new(
            "t",
            1,
            InspectorEvent::FuseStart {
                trace_id: 1,
                op: "lookup".into(),
                mount: "dns".into(),
                path: "/x".into(),
            },
        );
        let json = serde_json::to_string(&record).expect("json");
        fs::write(file.path(), format!("{json}\n")).expect("write");
        run_plain(SourceKind::Replay(file.path().to_path_buf())).expect("replay");
    }

    #[test]
    fn record_appends_while_emitting() {
        let mut capture = NamedTempFile::new().expect("tempfile");
        let json = r#"{"v":1,"ts":"t","mono_us":1,"event":{"type":"fuse.start","trace_id":1,"op":"read","mount":"m","path":"/"}}"#;
        emit_plain_line(json, Some(capture.as_file_mut())).expect("emit");
        capture.as_file_mut().rewind().expect("rewind");
        let mut contents = String::new();
        capture
            .as_file()
            .read_to_string(&mut contents)
            .expect("read");
        assert!(contents.contains("fuse.start"));
    }
}
