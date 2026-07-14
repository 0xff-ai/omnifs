use crate::export::ReadOnlyExport;
use crate::protocol::attrs::op_name;
use crate::protocol::consts::{NFS4_OK, NFS4ERR_MINOR_VERS_MISMATCH};
use crate::protocol::xdr::{XdrError, XdrReader, XdrWriter, usize_to_u32};
use crate::trace::Trace;

pub(crate) struct CompoundState {
    pub(crate) current: Option<u64>,
    pub(crate) saved: Option<u64>,
    pub(crate) events: Vec<String>,
}

/// The short-lived decoder for one NFS COMPOUND request. It owns the reader,
/// export view, and mutable filehandle state for the request while persistent
/// protocol state remains behind `Export`.
pub(crate) struct CompoundDecoder<'a, 'record> {
    pub(super) reader: &'a mut XdrReader<'record>,
    pub(super) export: &'a dyn ReadOnlyExport,
    pub(super) state: &'a mut CompoundState,
}

pub(crate) fn handle_compound(
    reader: &mut XdrReader<'_>,
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
    let mut decoder = CompoundDecoder {
        reader,
        export,
        state: &mut state,
    };
    let mut results = Vec::new();
    let mut top_status = NFS4_OK;
    // op_name returns &'static str, so we can keep the per-op label list
    // free of heap allocation on the hot RPC path.
    let mut names: Vec<&'static str> = Vec::new();

    if minor != 0 {
        top_status = NFS4ERR_MINOR_VERS_MISMATCH;
    } else {
        for _ in 0..op_count {
            let op = decoder.reader.u32()?;
            names.push(op_name(op));
            let (status, result) = decoder.dispatch(op)?;
            results.push(result);
            if status != NFS4_OK {
                top_status = status;
                break;
            }
        }
    }

    trace.line(&format!(
        "compound xid={} tag={:?} minor={} ops={} status={} path=current:{:?} saved:{:?} events={}",
        xid,
        tag,
        minor,
        names.join(","),
        top_status,
        decoder.state.current,
        decoder.state.saved,
        if decoder.state.events.is_empty() {
            "-".to_string()
        } else {
            decoder.state.events.join(";")
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
