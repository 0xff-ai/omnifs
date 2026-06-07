//! Event sources: replay file, live TCP subscriber.

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result};

/// TCP loopback address that `omnifs inspect` connects to by default.
/// Mirrors the port the daemon's `OMNIFS_INSPECTOR_ADDR` binds (and the
/// port `omnifs dev` forwards through Docker). Both sides keep their
/// own constant because the bind side is `0.0.0.0:...` and the
/// client side is `127.0.0.1:...`.
pub fn default_inspector_addr() -> SocketAddr {
    "127.0.0.1:7878"
        .parse()
        .expect("valid hard-coded loopback addr")
}

pub enum SourceKind {
    Replay(PathBuf),
    /// Connect to the daemon's inspector TCP socket. Optional
    /// `record` also appends every line read to a host-side file.
    Socket {
        addr: SocketAddr,
        record: Option<PathBuf>,
    },
}

/// Out-of-band signal the source thread sends so the front-end can
/// surface honest connection state instead of silently looping on a
/// failed `connect_timeout`.
pub enum SourceMessage {
    Line(String),
    /// First successful TCP connect, or a successful reconnect after a drop.
    Connected,
    /// Stream closed after a previously-connected session (daemon
    /// shutdown or transient drop). Reconnection attempts continue.
    Disconnected,
}

pub struct EventSource {
    rx: Receiver<SourceMessage>,
    handle: Option<JoinHandle<()>>,
}

impl EventSource {
    pub fn spawn(kind: SourceKind) -> Self {
        let (tx, rx) = mpsc::channel();
        let handle = match kind {
            SourceKind::Replay(path) => Some(thread::spawn(move || replay_path(&path, &tx))),
            SourceKind::Socket { addr, record } => {
                // The live socket source reconnects forever; detach it
                // so quitting the TUI never waits on the reconnect loop.
                let handle = thread::spawn(move || socket_source(addr, record.as_deref(), &tx));
                drop(handle);
                None
            },
        };
        Self { rx, handle }
    }

    pub fn drain(&self) -> Vec<SourceMessage> {
        let mut messages = Vec::new();
        while let Ok(message) = self.rx.try_recv() {
            messages.push(message);
        }
        messages
    }
}

impl Drop for EventSource {
    fn drop(&mut self) {
        // Close the receiver before joining finite replay workers so
        // they break out on their next send instead of finishing replay.
        let (_tx, rx) = mpsc::channel();
        drop(std::mem::replace(&mut self.rx, rx));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn replay_path(path: &Path, tx: &Sender<SourceMessage>) {
    let Ok(file) = File::open(path) else {
        return;
    };
    let reader = BufReader::new(file);
    for line in reader.lines().map_while(Result::ok) {
        if tx.send(SourceMessage::Line(line)).is_err() {
            break;
        }
        thread::sleep(Duration::from_millis(120));
    }
}

/// Connect to the daemon's TCP loopback and forward every received
/// line into `tx`. Reconnects with a short backoff if the daemon is
/// not yet listening — useful for `omnifs inspect` racing
/// `omnifs dev`. Connect/disconnect transitions are reported through
/// `SourceMessage` so the front-end never claims "connected" while the
/// socket is still failing.
fn socket_source(addr: SocketAddr, record: Option<&Path>, tx: &Sender<SourceMessage>) {
    let mut record_file = record.and_then(|path| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()
    });

    loop {
        let Ok(stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(500)) else {
            thread::sleep(Duration::from_millis(250));
            continue;
        };
        if tx.send(SourceMessage::Connected).is_err() {
            return;
        }
        let reader = BufReader::new(stream);
        for line in reader.lines().map_while(Result::ok) {
            if let Some(file) = record_file.as_mut() {
                let _ = writeln!(file, "{}", line.trim());
                let _ = file.flush();
            }
            if tx.send(SourceMessage::Line(line)).is_err() {
                return;
            }
        }
        // Stream closed (daemon shutdown or transient drop). Brief
        // backoff then try to reconnect; the daemon will restart with
        // a fresh history snapshot.
        if tx.send(SourceMessage::Disconnected).is_err() {
            return;
        }
        thread::sleep(Duration::from_millis(500));
    }
}

pub fn replay_file_blocking(path: &Path) -> Result<Vec<String>> {
    let file = File::open(path).with_context(|| format!("open `{}`", path.display()))?;
    Ok(BufReader::new(file).lines().map_while(Result::ok).collect())
}
