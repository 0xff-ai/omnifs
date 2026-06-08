use crate::export::{Status, StatusResult};
use std::sync::OnceLock;
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
    static INSTANCE: OnceLock<u64> = OnceLock::new();
    *INSTANCE.get_or_init(|| {
        let mut bytes = [0_u8; 8];
        getrandom::fill(&mut bytes).expect("OS randomness is required for NFS filehandles");
        let value = u64::from_be_bytes(bytes);
        if value == 0 { 1 } else { value }
    })
}

#[cfg(test)]
pub(crate) fn client_id(generation: u64) -> u64 {
    client_id_for_slot(generation, 1)
}

pub(crate) fn client_id_for_slot(generation: u64, slot: u64) -> u64 {
    let id = generation ^ 0x4f4d_4e49_4653_0001 ^ slot.rotate_left(17);
    if id == 0 { 1 } else { id }
}

pub(crate) fn file_handle(generation: u64, id: u64) -> Vec<u8> {
    let mut fh = Vec::with_capacity(16);
    fh.extend_from_slice(&generation.to_be_bytes());
    fh.extend_from_slice(&id.to_be_bytes());
    fh
}

pub(crate) fn decode_file_handle(generation: u64, fh: &[u8]) -> StatusResult<u64> {
    if fh.len() != 16 {
        return Err(Status::BadHandle);
    }
    let mut gen_bytes = [0_u8; 8];
    gen_bytes.copy_from_slice(&fh[..8]);
    if u64::from_be_bytes(gen_bytes) != generation {
        return Err(Status::FhExpired);
    }
    let mut id = [0_u8; 8];
    id.copy_from_slice(&fh[8..]);
    Ok(u64::from_be_bytes(id))
}
