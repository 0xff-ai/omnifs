use crate::export::ReadOnlyExport;
use crate::protocol::attrs::{op_name, op_status};
use crate::protocol::consts::{NFS4_OK, NFS4ERR_MINOR_VERS_MISMATCH};
use crate::protocol::ops::handle_op;
use crate::protocol::xdr::{XdrError, XdrReader, XdrWriter, usize_to_u32};
use crate::trace::Trace;

pub(crate) struct CompoundState {
    pub(crate) current: Option<u64>,
    pub(crate) saved: Option<u64>,
    pub(crate) events: Vec<String>,
}

pub(crate) fn handle_compound(
    reader: &mut XdrReader<'_>,
    generation: u64,
    export: &dyn ReadOnlyExport,
    xid: u32,
    trace: &Trace,
) -> Result<Vec<u8>, XdrError> {
    let tag = reader.string()?;
    let minor = reader.u32()?;
    let op_count = reader.u32()?;
    let mut state = CompoundState {
        current: None,
        saved: None,
        events: Vec::new(),
    };
    let mut results = Vec::new();
    let mut top_status = NFS4_OK;
    let mut names = Vec::new();

    if minor != 0 {
        top_status = NFS4ERR_MINOR_VERS_MISMATCH;
        if op_count > 0 {
            let op = reader.u32()?;
            names.push(op_name(op).to_string());
            results.push(op_status(op, NFS4ERR_MINOR_VERS_MISMATCH).into_inner());
        }
    } else {
        for _ in 0..op_count {
            let op = reader.u32()?;
            names.push(op_name(op).to_string());
            let (status, result) = handle_op(op, reader, generation, export, &mut state)?;
            results.push(result);
            if status != NFS4_OK {
                top_status = status;
                break;
            }
        }
    }

    trace.line(format!(
        "compound xid={} tag={:?} minor={} ops={} status={} path=current:{:?} saved:{:?} events={}",
        xid,
        tag,
        minor,
        names.join(","),
        top_status,
        state.current,
        state.saved,
        if state.events.is_empty() {
            "-".to_string()
        } else {
            state.events.join(";")
        }
    ));

    let mut out = XdrWriter::new();
    out.u32(top_status);
    out.string(&tag);
    out.u32(usize_to_u32(results.len()));
    for result in results {
        out.bytes(&result);
    }
    Ok(out.into_inner())
}
