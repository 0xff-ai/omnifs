//! Length-delimited framing for the Omnifs VFS wire protocol.
//!
//! On-stream layout, exactly:
//!
//! ```text
//! u32 len (LE) | u64 request_id (LE) | u8 kind | postcard body
//! ```
//!
//! `len` counts the bytes that follow it: `request_id` (8) + `kind` (1) +
//! `body`. A frame therefore always has `len >= 9`. A `len` above
//! [`MAX_FRAME`] is a protocol violation that kills the connection rather than
//! allocating an attacker-chosen buffer, so the guard runs before the body read.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::WireError;

/// A request frame: a client-issued [`WireRequest`](crate::WireRequest), or the
/// `Hello` handshake.
pub(crate) const KIND_REQUEST: u8 = 0;
/// A response frame: a server-issued [`WireResponse`](crate::WireResponse), or
/// the `Welcome` handshake.
pub(crate) const KIND_RESPONSE: u8 = 1;
/// An event frame: a server-pushed [`NsEvent`](omnifs_engine::NsEvent), always
/// carried with `request_id = 0`.
pub(crate) const KIND_EVENT: u8 = 2;

/// Maximum accepted frame size (`len` field), 16 MiB. A larger `len` is a
/// protocol error; the connection is dropped.
pub(crate) const MAX_FRAME: u32 = 16 * 1024 * 1024;

/// The fixed overhead `len` accounts for beyond the body: `request_id` + `kind`.
const HEADER_TAIL: u32 = 9;

/// One decoded frame. The body is the postcard encoding of the payload the
/// `kind` selects.
#[derive(Debug, Clone)]
pub(crate) struct Frame {
    pub request_id: u64,
    pub kind: u8,
    pub body: Vec<u8>,
}

impl Frame {
    pub(crate) fn new(request_id: u64, kind: u8, body: Vec<u8>) -> Self {
        Self {
            request_id,
            kind,
            body,
        }
    }
}

/// Read one frame. `Ok(None)` is a clean end of stream (the peer closed between
/// frames); every other short read or oversized `len` is a [`WireError`] that
/// the caller treats as a dropped connection.
pub(crate) async fn read_frame<R>(reader: &mut R) -> Result<Option<Frame>, WireError>
where
    R: AsyncRead + Unpin,
{
    let mut len_buf = [0_u8; 4];
    match reader.read_exact(&mut len_buf).await {
        Ok(_) => {},
        // A peer that closed between frames reports EOF on the length read; that
        // is an orderly disconnect, not a protocol error.
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(WireError::Io(error)),
    }
    let len = u32::from_le_bytes(len_buf);
    if len < HEADER_TAIL {
        return Err(WireError::Protocol(format!(
            "frame len {len} below the {HEADER_TAIL}-byte header floor"
        )));
    }
    if len > MAX_FRAME {
        return Err(WireError::FrameTooLarge { len });
    }
    let request_id = reader.read_u64_le().await?;
    let kind = reader.read_u8().await?;
    let body_len = (len - HEADER_TAIL) as usize;
    let mut body = vec![0_u8; body_len];
    reader.read_exact(&mut body).await?;
    Ok(Some(Frame {
        request_id,
        kind,
        body,
    }))
}

/// Write one frame and flush it. A body that would overflow [`MAX_FRAME`] is a
/// local bug (an oversized answer), reported rather than emitted.
pub(crate) async fn write_frame<W>(writer: &mut W, frame: &Frame) -> Result<(), WireError>
where
    W: AsyncWrite + Unpin,
{
    let body_len = u32::try_from(frame.body.len())
        .ok()
        .filter(|len| *len <= MAX_FRAME - HEADER_TAIL)
        .ok_or(WireError::FrameTooLarge {
            len: MAX_FRAME.saturating_add(1),
        })?;
    let len = body_len + HEADER_TAIL;
    writer.write_u32_le(len).await?;
    writer.write_u64_le(frame.request_id).await?;
    writer.write_u8(frame.kind).await?;
    writer.write_all(&frame.body).await?;
    writer.flush().await?;
    Ok(())
}
