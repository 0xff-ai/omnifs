//! Raw-byte tailing for the daemon log.
//!
//! Logs preserve raw bytes: nothing here decodes, re-encodes, or normalizes
//! line endings. The tail window is bounded so a client asking for a few
//! lines never forces a read of an arbitrarily large log.

use omnifs_api::CONTROL_STREAM_PAYLOAD_MAX_BYTES;
use omnifs_api::grpc::wire;
use std::path::PathBuf;
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};
use tonic::Status;

/// The most log a single request will scan backwards for its tail.
const SCAN_MAX_BYTES: u64 = 16 * 1024 * 1024;
const READ_CHUNK_BYTES: usize = 64 * 1024;
const FOLLOW_POLL: std::time::Duration = std::time::Duration::from_millis(100);

type Sender = tokio::sync::mpsc::Sender<Result<wire::LogStreamItem, Status>>;

/// Keep the last `tail_lines` lines of `bytes`, preserving raw bytes exactly.
///
/// A trailing newline terminates the final line rather than starting an empty
/// one, so `tail_lines` counts lines a reader would see.
pub(crate) fn tail_bytes(bytes: &[u8], tail_lines: usize) -> Vec<u8> {
    let end = if bytes.last() == Some(&b'\n') {
        bytes.len().saturating_sub(1)
    } else {
        bytes.len()
    };
    let start = tail_lines
        .checked_sub(1)
        .and_then(|skip| {
            bytes[..end]
                .iter()
                .enumerate()
                .rev()
                .filter(|(_, byte)| **byte == b'\n')
                .nth(skip)
        })
        .map_or(0, |(index, _)| index + 1);
    bytes[start..].to_vec()
}

/// Read the bounded tail window of an open log file.
///
/// Seeks back at most [`SCAN_MAX_BYTES`] and, when that lands mid-line,
/// drops the partial first line so the caller never emits a fragment.
/// Leaves the cursor at end of file, ready for follow mode.
async fn read_tail_window(
    file: &mut tokio::fs::File,
    size: u64,
    tail_lines: usize,
) -> std::io::Result<Vec<u8>> {
    let start = size.saturating_sub(SCAN_MAX_BYTES);
    file.seek(std::io::SeekFrom::Start(start)).await?;
    let window = size - start;
    let mut scanned = Vec::with_capacity(window.try_into().unwrap_or(0));
    let mut buffer = vec![0_u8; READ_CHUNK_BYTES];
    while scanned.len() < usize::try_from(window).unwrap_or(usize::MAX) {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        scanned.extend_from_slice(&buffer[..read]);
    }
    if start > 0
        && let Some(first) = scanned.iter().position(|byte| *byte == b'\n')
    {
        scanned.drain(..=first);
    }
    Ok(tail_bytes(&scanned, tail_lines))
}

fn data_item(chunk: &[u8]) -> wire::LogStreamItem {
    wire::LogStreamItem {
        value: Some(wire::log_stream_item::Value::Data(chunk.to_vec().into())),
    }
}

/// Send `bytes` split into wire-sized payloads. Returns `false` once the
/// client has gone away.
async fn send_chunks(sender: &Sender, bytes: &[u8]) -> bool {
    for chunk in bytes.chunks(CONTROL_STREAM_PAYLOAD_MAX_BYTES) {
        if sender.send(Ok(data_item(chunk))).await.is_err() {
            return false;
        }
    }
    true
}

/// Stream the log at `path`: its bounded tail first, then appended bytes
/// while `follow` holds, until the client disconnects or the daemon stops.
///
/// A missing log is not an error; it streams nothing and ends.
pub(crate) async fn stream(
    path: PathBuf,
    tail_lines: usize,
    follow: bool,
    sender: Sender,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let mut file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            let _ = sender.send(Err(crate::control::grpc_internal(error))).await;
            return;
        },
    };
    let size = match file.metadata().await {
        Ok(metadata) => metadata.len(),
        Err(error) => {
            let _ = sender.send(Err(crate::control::grpc_internal(error))).await;
            return;
        },
    };
    let Ok(tail) = read_tail_window(&mut file, size, tail_lines).await else {
        return;
    };
    if !send_chunks(&sender, &tail).await || !follow {
        return;
    }
    let mut buffer = vec![0_u8; READ_CHUNK_BYTES];
    loop {
        let Ok(read) = file.read(&mut buffer).await else {
            return;
        };
        if read > 0 {
            if !send_chunks(&sender, &buffer[..read]).await {
                return;
            }
            continue;
        }
        tokio::select! {
            () = sender.closed() => return,
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() { return; }
            },
            () = tokio::time::sleep(FOLLOW_POLL) => {},
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_api::CONTROL_STREAM_ITEM_MAX_BYTES;
    use prost::Message as _;
    use tokio::io::AsyncWriteExt as _;

    /// A full-size payload plus its framing must still fit one stream item,
    /// or the largest chunk this module emits would be unencodable.
    #[test]
    fn largest_log_item_fits_encoding_limit() {
        let item = data_item(&vec![0xff; CONTROL_STREAM_PAYLOAD_MAX_BYTES]);
        assert!(item.encoded_len() <= CONTROL_STREAM_ITEM_MAX_BYTES);
    }

    #[test]
    fn tail_preserves_raw_line_bytes() {
        assert_eq!(tail_bytes(b"one\ntwo\nthree\n", 2), b"two\nthree\n");
        assert_eq!(tail_bytes(b"one\ntwo\nthree", 1), b"three");
        assert_eq!(tail_bytes(b"one\ntwo", 99), b"one\ntwo");
    }

    async fn window(contents: &[u8], tail_lines: usize) -> Vec<u8> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.log");
        let mut file = tokio::fs::File::create(&path).await.unwrap();
        file.write_all(contents).await.unwrap();
        file.flush().await.unwrap();
        let mut file = tokio::fs::File::open(&path).await.unwrap();
        let size = file.metadata().await.unwrap().len();
        read_tail_window(&mut file, size, tail_lines).await.unwrap()
    }

    #[tokio::test]
    async fn window_reads_the_requested_tail() {
        assert_eq!(window(b"a\nb\nc\n", 2).await, b"b\nc\n");
        assert_eq!(window(b"a\nb\nc\n", 99).await, b"a\nb\nc\n");
    }

    /// A log larger than the scan bound is read from a mid-line offset, so
    /// the partial first line has to go or the client sees a fragment.
    #[tokio::test]
    async fn window_drops_the_partial_line_at_the_scan_bound() {
        let mut log = vec![b'x'; usize::try_from(SCAN_MAX_BYTES).unwrap()];
        log.extend_from_slice(b"\nlast line\n");
        let tail = window(&log, 99).await;
        assert_eq!(tail, b"last line\n");
        assert!(!tail.contains(&b'x'), "partial first line survived");
    }

    /// Reading must not stop at the first short read, or a chunk-boundary
    /// read would silently truncate the tail.
    #[tokio::test]
    async fn window_spans_many_read_chunks() {
        let mut log = Vec::new();
        for i in 0..(READ_CHUNK_BYTES / 8) {
            log.extend_from_slice(format!("line{i}\n").as_bytes());
        }
        let tail = window(&log, 1).await;
        assert_eq!(
            tail,
            format!("line{}\n", READ_CHUNK_BYTES / 8 - 1).as_bytes()
        );
    }
}
