use crate::export::NfsResult;
use crate::protocol::consts::{NFS4ERR_BADHANDLE, NFS4ERR_STALE};
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) fn now_sec() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    )
    .unwrap_or(i64::MAX)
}

pub(crate) fn generation() -> u64 {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    duration
        .as_secs()
        .wrapping_mul(1_000_000_000)
        .wrapping_add(u64::from(duration.subsec_nanos()))
}

pub(crate) fn file_handle(generation: u64, id: u64) -> Vec<u8> {
    let mut fh = Vec::with_capacity(16);
    fh.extend_from_slice(&generation.to_be_bytes());
    fh.extend_from_slice(&id.to_be_bytes());
    fh
}

pub(crate) fn decode_file_handle(generation: u64, fh: &[u8]) -> NfsResult<u64> {
    if fh.len() != 16 {
        return Err(NFS4ERR_BADHANDLE);
    }
    let mut gen_bytes = [0_u8; 8];
    gen_bytes.copy_from_slice(&fh[..8]);
    if u64::from_be_bytes(gen_bytes) != generation {
        return Err(NFS4ERR_STALE);
    }
    let mut id = [0_u8; 8];
    id.copy_from_slice(&fh[8..]);
    Ok(u64::from_be_bytes(id))
}
