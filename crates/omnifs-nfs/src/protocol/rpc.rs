use crate::export::ReadOnlyExport;
use crate::protocol::compound::handle_compound;
use crate::protocol::consts::{
    AUTH_NONE, NFS_PROGRAM, NFS_VERSION_4, PROC_COMPOUND, RPC_CALL, RPC_MSG_ACCEPTED, RPC_REPLY,
    RPC_SUCCESS,
};
use crate::protocol::xdr::{XdrReader, XdrWriter};
use crate::trace::Trace;
use std::io::{self, Read, Write};
use std::net::TcpStream;

pub(crate) fn read_rpc_record(stream: &mut TcpStream) -> io::Result<Option<Vec<u8>>> {
    let mut record = Vec::new();
    loop {
        let mut header = [0_u8; 4];
        match stream.read_exact(&mut header) {
            Ok(()) => {},
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(error) if error.kind() == io::ErrorKind::ConnectionReset => return Ok(None),
            Err(error) => return Err(error),
        }
        let marker = u32::from_be_bytes(header);
        let last = marker & 0x8000_0000 != 0;
        let len = (marker & 0x7fff_ffff) as usize;
        let start = record.len();
        record.resize(start + len, 0);
        stream.read_exact(&mut record[start..])?;
        if last {
            return Ok(Some(record));
        }
    }
}

pub(crate) fn write_rpc_record(stream: &mut TcpStream, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "RPC record payload exceeds u32 length",
        )
    })?;
    if len > 0x7fff_ffff {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "RPC record payload exceeds ONC RPC fragment length",
        ));
    }
    let marker = 0x8000_0000_u32 | len;
    stream.write_all(&marker.to_be_bytes())?;
    stream.write_all(payload)?;
    stream.flush()
}

pub(crate) fn handle_rpc_record(
    record: &[u8],
    generation: u64,
    export: &dyn ReadOnlyExport,
    trace: &Trace,
) -> Vec<u8> {
    let mut reader = XdrReader::new(record);
    let xid = reader.u32().unwrap_or_default();
    let msg_type = reader.u32().unwrap_or_default();
    let rpc_version = reader.u32().unwrap_or_default();
    let program = reader.u32().unwrap_or_default();
    let version = reader.u32().unwrap_or_default();
    let procedure = reader.u32().unwrap_or_default();

    let cred_flavor = reader.u32().unwrap_or_default();
    let _cred = reader.opaque().unwrap_or_default();
    let verifier_flavor = reader.u32().unwrap_or_default();
    let _verifier = reader.opaque().unwrap_or_default();

    trace.line(format!(
        "rpc xid={xid} program={program} version={version} procedure={procedure} cred={cred_flavor} verf={verifier_flavor}"
    ));

    let mut reply_body = XdrWriter::new();
    if msg_type != RPC_CALL
        || rpc_version != 2
        || program != NFS_PROGRAM
        || version != NFS_VERSION_4
    {
        return rpc_reply(xid, &reply_body.into_inner());
    }

    if procedure == PROC_COMPOUND {
        match handle_compound(&mut reader, generation, export, xid, trace) {
            Ok(compound) => reply_body.bytes(&compound),
            Err(error) => {
                trace.line(format!("compound_decode_error xid={xid} err={error}"));
            },
        }
    }

    rpc_reply(xid, &reply_body.into_inner())
}

fn rpc_reply(xid: u32, body: &[u8]) -> Vec<u8> {
    let mut out = XdrWriter::new();
    out.u32(xid);
    out.u32(RPC_REPLY);
    out.u32(RPC_MSG_ACCEPTED);
    out.u32(AUTH_NONE);
    out.u32(0);
    out.u32(RPC_SUCCESS);
    out.bytes(body);
    out.into_inner()
}
