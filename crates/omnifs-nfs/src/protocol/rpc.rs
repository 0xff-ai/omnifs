use crate::export::ReadOnlyExport;
use crate::protocol::client::ClientTable;
use crate::protocol::compound::handle_compound;
use crate::protocol::consts::{
    AUTH_BADCRED, AUTH_ERROR, AUTH_NONE, AUTH_SYS, MAX_RPC_RECORD_BYTES, NFS_PROGRAM,
    NFS_VERSION_4, NFS_VERSION_MAX, NFS_VERSION_MIN, PROC_COMPOUND, PROC_NULL, RPC_CALL,
    RPC_GARBAGE_ARGS, RPC_MISMATCH, RPC_MSG_ACCEPTED, RPC_MSG_DENIED, RPC_PROC_UNAVAIL,
    RPC_PROG_MISMATCH, RPC_PROG_UNAVAIL, RPC_REPLY, RPC_SUCCESS,
};
use crate::protocol::xdr::{XdrReader, XdrWriter};
use crate::trace::Trace;
use std::io::{self, Read, Write};
use std::net::TcpStream;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AcceptStat {
    Success,
    ProgUnavailable,
    ProgMismatch,
    ProcUnavailable,
    GarbageArgs,
}

impl AcceptStat {
    fn wire(self) -> u32 {
        match self {
            Self::Success => RPC_SUCCESS,
            Self::ProgUnavailable => RPC_PROG_UNAVAIL,
            Self::ProgMismatch => RPC_PROG_MISMATCH,
            Self::ProcUnavailable => RPC_PROC_UNAVAIL,
            Self::GarbageArgs => RPC_GARBAGE_ARGS,
        }
    }
}

/// Read one complete ONC RPC record (potentially multiple fragments).
///
/// Rejects records whose cumulative size exceeds `MAX_RPC_RECORD_BYTES`
/// (16 MiB) so a malicious or misconfigured peer cannot exhaust memory
/// with an unbounded stream of non-last fragments.
pub(crate) fn read_rpc_record(stream: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
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
        let new_len = record.len().saturating_add(len);
        if new_len as u64 > MAX_RPC_RECORD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("RPC record exceeded maximum size of {MAX_RPC_RECORD_BYTES} bytes"),
            ));
        }
        let start = record.len();
        record.resize(new_len, 0);
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
    clients: &ClientTable,
    export: &dyn ReadOnlyExport,
    trace: &Trace,
) -> Vec<u8> {
    let mut reader = XdrReader::new(record);

    let Ok((xid, msg_type, rpc_version, program, version, procedure)) =
        parse_call_header(&mut reader)
    else {
        return rpc_reply_accepted(0, AcceptStat::GarbageArgs, &[]);
    };

    let Ok((cred_flavor, _cred_body, verifier_flavor, _verifier_body)) = parse_auth(&mut reader)
    else {
        trace.line(&format!("rpc xid={xid} truncated auth (GARBAGE_ARGS)"));
        return rpc_reply_accepted(xid, AcceptStat::GarbageArgs, &[]);
    };

    // Non-CALL messages: treat as GARBAGE_ARGS.
    if msg_type != RPC_CALL {
        trace.line(&format!("rpc xid={xid} msg_type={msg_type} (not a CALL)"));
        return rpc_reply_accepted(xid, AcceptStat::GarbageArgs, &[]);
    }

    // RPC version mismatch: denied reply with RPC_MISMATCH body.
    if rpc_version != 2 {
        trace.line(&format!(
            "rpc xid={xid} rpc_version={rpc_version} (RPC_MISMATCH)"
        ));
        return rpc_reply_denied_rpc_mismatch(xid, 2, 2);
    }

    // Validate credential flavor. Loopback-only server accepts AUTH_SYS
    // and AUTH_NONE; unknown flavors get a denied AUTH_ERROR reply.
    if ![AUTH_SYS, AUTH_NONE].contains(&cred_flavor) {
        trace.line(&format!("rpc xid={xid} cred={cred_flavor} rejected"));
        return rpc_reply_denied_auth_error(xid, AUTH_BADCRED);
    }

    trace.line(&format!(
        "rpc xid={xid} program={program} version={version} procedure={procedure} cred={cred_flavor} verf={verifier_flavor}"
    ));

    if program != NFS_PROGRAM {
        return rpc_reply_accepted(xid, AcceptStat::ProgUnavailable, &[]);
    }

    if version != NFS_VERSION_4 {
        return rpc_reply_prog_mismatch(xid, NFS_VERSION_MIN, NFS_VERSION_MAX);
    }

    if procedure == PROC_NULL {
        trace.line(&format!("rpc xid={xid} NULLPROC"));
        return rpc_reply_accepted(xid, AcceptStat::Success, &[]);
    }

    if procedure != PROC_COMPOUND {
        trace.line(&format!("rpc xid={xid} procedure={procedure} PROC_UNAVAIL"));
        return rpc_reply_accepted(xid, AcceptStat::ProcUnavailable, &[]);
    }

    match handle_compound(&mut reader, generation, clients, export, xid, trace) {
        Ok(compound) => rpc_reply_accepted(xid, AcceptStat::Success, &compound),
        Err(error) => {
            trace.line(&format!(
                "compound_decode_error xid={xid} err={error} (GARBAGE_ARGS)"
            ));
            rpc_reply_accepted(xid, AcceptStat::GarbageArgs, &[])
        },
    }
}

// ---------------------------------------------------------------------------
// Reply constructors
//
// RFC 5531 section 9 wire layouts:
//
//   MSG_ACCEPTED: xid REPLY MSG_ACCEPTED verf(flavor+len+body) accept_stat [body]
//   MSG_DENIED:   xid REPLY MSG_DENIED   reject_stat [union body]
//
//   reject_stat=RPC_MISMATCH  -> body: low(u32) high(u32)
//   reject_stat=AUTH_ERROR    -> body: auth_stat(u32)
// ---------------------------------------------------------------------------

fn rpc_reply_accepted(xid: u32, accept_stat: AcceptStat, body: &[u8]) -> Vec<u8> {
    let mut out = XdrWriter::new();
    out.u32(xid);
    out.u32(RPC_REPLY);
    out.u32(RPC_MSG_ACCEPTED);
    // AUTH_NONE verifier: flavor + zero-length body.
    out.u32(AUTH_NONE);
    out.u32(0);
    out.u32(accept_stat.wire());
    out.bytes(body);
    out.into_inner()
}

fn rpc_reply_denied_rpc_mismatch(xid: u32, low: u32, high: u32) -> Vec<u8> {
    let mut out = XdrWriter::new();
    out.u32(xid);
    out.u32(RPC_REPLY);
    out.u32(RPC_MSG_DENIED);
    out.u32(RPC_MISMATCH);
    out.u32(low);
    out.u32(high);
    out.into_inner()
}

fn rpc_reply_denied_auth_error(xid: u32, auth_stat: u32) -> Vec<u8> {
    let mut out = XdrWriter::new();
    out.u32(xid);
    out.u32(RPC_REPLY);
    out.u32(RPC_MSG_DENIED);
    out.u32(AUTH_ERROR);
    out.u32(auth_stat);
    out.into_inner()
}

fn rpc_reply_prog_mismatch(xid: u32, low: u32, high: u32) -> Vec<u8> {
    let mut body = XdrWriter::new();
    body.u32(low);
    body.u32(high);
    rpc_reply_accepted(xid, AcceptStat::ProgMismatch, &body.into_inner())
}

// ---------------------------------------------------------------------------
// Strict call-header parser: returns Err on any XDR underflow so the
// caller can emit GARBAGE_ARGS instead of silently defaulting fields.
// ---------------------------------------------------------------------------

fn parse_call_header(reader: &mut XdrReader<'_>) -> Result<(u32, u32, u32, u32, u32, u32), ()> {
    let xid = reader.u32().map_err(|_| ())?;
    let msg_type = reader.u32().map_err(|_| ())?;
    // Only parse the remaining header fields if msg_type is CALL;
    // otherwise leave them as 0 and let the caller decide.
    if msg_type != RPC_CALL {
        return Ok((xid, msg_type, 0, 0, 0, 0));
    }
    let rpc_version = reader.u32().map_err(|_| ())?;
    let program = reader.u32().map_err(|_| ())?;
    let version = reader.u32().map_err(|_| ())?;
    let procedure = reader.u32().map_err(|_| ())?;
    Ok((xid, msg_type, rpc_version, program, version, procedure))
}

fn parse_auth(reader: &mut XdrReader<'_>) -> Result<(u32, Vec<u8>, u32, Vec<u8>), ()> {
    let cred_flavor = reader.u32().map_err(|_| ())?;
    let cred_body = reader.opaque().map_err(|_| ())?;
    let verifier_flavor = reader.u32().map_err(|_| ())?;
    let verifier_body = reader.opaque().map_err(|_| ())?;
    Ok((cred_flavor, cred_body, verifier_flavor, verifier_body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::{
        Attr, DirListing, OpenRead, OpenResult, ReadOnlyExport, StateId, Status, StatusResult,
    };
    use crate::protocol::client::ClientTable;
    use crate::protocol::xdr::XdrWriter;

    struct NullExport;
    impl ReadOnlyExport for NullExport {
        fn root(&self) -> u64 {
            0
        }
        fn attr(&self, _id: u64) -> StatusResult<Attr> {
            Err(Status::Stale)
        }
        fn lookup(&self, _parent: u64, _name: &str) -> StatusResult<u64> {
            Err(Status::NoEnt)
        }
        fn readdir(&self, _id: u64) -> StatusResult<DirListing> {
            Err(Status::NotDir)
        }
        fn read(&self, _id: u64) -> StatusResult<Vec<u8>> {
            Err(Status::Invalid)
        }
        fn readlink(&self, _id: u64) -> StatusResult<Vec<u8>> {
            Err(Status::Invalid)
        }
        fn open_state(
            &self,
            _generation: u64,
            _id: u64,
            _clientid: u64,
            _access: u32,
        ) -> StatusResult<OpenResult> {
            Err(Status::Invalid)
        }
        fn validate_state(&self, _stateid: StateId) -> StatusResult<()> {
            Err(Status::BadStateId)
        }
        fn read_state(
            &self,
            _stateid: StateId,
            _offset: u64,
            _count: u32,
        ) -> StatusResult<OpenRead> {
            Err(Status::BadStateId)
        }
        fn close_state(&self, _stateid: StateId) -> StatusResult<StateId> {
            Err(Status::BadStateId)
        }
        fn renew_client(&self, _clientid: u64) -> StatusResult<()> {
            Err(Status::StaleClientId)
        }
    }

    fn trace() -> Trace {
        Trace::new(None).unwrap()
    }

    fn clients() -> ClientTable {
        ClientTable::with_confirmed_default(0)
    }

    fn build_rpc_call(
        xid: u32,
        program: u32,
        version: u32,
        procedure: u32,
        body: &[u8],
    ) -> Vec<u8> {
        let mut w = XdrWriter::new();
        w.u32(xid);
        w.u32(RPC_CALL);
        w.u32(2); // RPC version
        w.u32(program);
        w.u32(version);
        w.u32(procedure);
        // AUTH_SYS with empty body
        w.u32(AUTH_SYS);
        w.u32(0);
        w.u32(AUTH_NONE);
        w.u32(0);
        w.bytes(body);
        w.into_inner()
    }

    fn reply_accept_stat(reply: &[u8]) -> Option<u32> {
        let mut r = XdrReader::new(reply);
        let _xid = r.u32().ok()?;
        let _msg_type = r.u32().ok()?;
        let reply_stat = r.u32().ok()?;
        if reply_stat != RPC_MSG_ACCEPTED {
            return None;
        }
        // skip verifier
        let _vf = r.u32().ok()?;
        let _vl = r.u32().ok()?;
        r.u32().ok()
    }

    fn reply_reject_stat(reply: &[u8]) -> Option<u32> {
        let mut r = XdrReader::new(reply);
        let _xid = r.u32().ok()?;
        let _msg_type = r.u32().ok()?;
        let reply_stat = r.u32().ok()?;
        if reply_stat != RPC_MSG_DENIED {
            return None;
        }
        r.u32().ok()
    }

    #[test]
    fn rpc_dispatch_accept_stats() {
        let export = NullExport;
        for (xid, program, version, proc, expected) in [
            (1, NFS_PROGRAM, NFS_VERSION_4, PROC_NULL, RPC_SUCCESS),
            (2, NFS_PROGRAM, NFS_VERSION_4, 99, RPC_PROC_UNAVAIL),
            (3, 999_999, NFS_VERSION_4, PROC_NULL, RPC_PROG_UNAVAIL),
        ] {
            let call = build_rpc_call(xid, program, version, proc, &[]);
            let reply = handle_rpc_record(&call, 0, &clients(), &export, &trace());
            assert_eq!(
                reply_accept_stat(&reply),
                Some(expected),
                "xid={xid} program={program} proc={proc}"
            );
        }
    }

    #[test]
    fn wrong_nfs_version_returns_prog_mismatch_with_range() {
        let export = NullExport;
        let call = build_rpc_call(4, NFS_PROGRAM, 3, PROC_NULL, &[]);
        let reply = handle_rpc_record(&call, 0, &clients(), &export, &trace());
        assert_eq!(reply_accept_stat(&reply), Some(RPC_PROG_MISMATCH));
        // Body should contain low=4, high=4.
        let mut r = XdrReader::new(&reply);
        r.u32().unwrap(); // xid
        r.u32().unwrap(); // REPLY
        r.u32().unwrap(); // MSG_ACCEPTED
        r.u32().unwrap(); // verifier flavor
        r.u32().unwrap(); // verifier len
        assert_eq!(r.u32().unwrap(), RPC_PROG_MISMATCH);
        assert_eq!(r.u32().unwrap(), 4); // low
        assert_eq!(r.u32().unwrap(), 4); // high
    }

    #[test]
    fn bad_credential_returns_auth_error_denied() {
        let export = NullExport;
        let mut w = XdrWriter::new();
        w.u32(5);
        w.u32(RPC_CALL);
        w.u32(2);
        w.u32(NFS_PROGRAM);
        w.u32(NFS_VERSION_4);
        w.u32(PROC_NULL);
        w.u32(6); // RPCSEC_GSS = unknown flavor
        w.u32(0);
        w.u32(AUTH_NONE);
        w.u32(0);
        let call = w.into_inner();
        let reply = handle_rpc_record(&call, 0, &clients(), &export, &trace());
        assert_eq!(reply_reject_stat(&reply), Some(AUTH_ERROR));
        // Body is auth_stat = AUTH_BADCRED.
        let mut r = XdrReader::new(&reply);
        r.u32().unwrap(); // xid
        r.u32().unwrap(); // REPLY
        r.u32().unwrap(); // MSG_DENIED
        assert_eq!(r.u32().unwrap(), AUTH_ERROR);
        assert_eq!(r.u32().unwrap(), AUTH_BADCRED);
    }

    #[test]
    fn rpc_version_mismatch_returns_denied_with_mismatch() {
        let export = NullExport;
        let mut w = XdrWriter::new();
        w.u32(6);
        w.u32(RPC_CALL);
        w.u32(3); // unsupported RPC version
        w.u32(NFS_PROGRAM);
        w.u32(NFS_VERSION_4);
        w.u32(PROC_NULL);
        w.u32(AUTH_SYS);
        w.u32(0);
        w.u32(AUTH_NONE);
        w.u32(0);
        let call = w.into_inner();
        let reply = handle_rpc_record(&call, 0, &clients(), &export, &trace());
        assert_eq!(reply_reject_stat(&reply), Some(RPC_MISMATCH));
        let mut r = XdrReader::new(&reply);
        r.u32().unwrap(); // xid
        r.u32().unwrap(); // REPLY
        r.u32().unwrap(); // MSG_DENIED
        assert_eq!(r.u32().unwrap(), RPC_MISMATCH);
        assert_eq!(r.u32().unwrap(), 2); // low
        assert_eq!(r.u32().unwrap(), 2); // high
    }

    #[test]
    fn truncated_rpc_header_returns_garbage_args() {
        let export = NullExport;
        let short = {
            let mut w = XdrWriter::new();
            w.u32(7);
            w.u32(RPC_CALL);
            w.into_inner()
        };
        let reply = handle_rpc_record(&short, 0, &clients(), &export, &trace());
        assert_eq!(reply_accept_stat(&reply), Some(RPC_GARBAGE_ARGS));

        let mut w = XdrWriter::new();
        w.u32(8);
        w.u32(RPC_CALL);
        w.u32(2);
        w.u32(NFS_PROGRAM);
        let truncated_auth = w.into_inner();
        let reply = handle_rpc_record(&truncated_auth, 0, &clients(), &export, &trace());
        assert_eq!(reply_accept_stat(&reply), Some(RPC_GARBAGE_ARGS));
    }

    #[test]
    fn malformed_compound_returns_garbage_args() {
        let export = NullExport;
        // Build a COMPOUND with op_count=5 but zero ops.
        let mut body = XdrWriter::new();
        body.string("test");
        body.u32(0); // minor version
        body.u32(5); // op_count=5
        // no ops follow; underflow on first op decode.
        let call = build_rpc_call(
            9,
            NFS_PROGRAM,
            NFS_VERSION_4,
            PROC_COMPOUND,
            &body.into_inner(),
        );
        let reply = handle_rpc_record(&call, 0, &clients(), &export, &trace());
        assert_eq!(reply_accept_stat(&reply), Some(RPC_GARBAGE_ARGS));
    }

    #[test]
    fn multi_fragment_record_concatenates_payload() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&3_u32.to_be_bytes());
        buf.extend_from_slice(b"abc");
        buf.extend_from_slice(&(0x8000_0000_u32 | 2).to_be_bytes());
        buf.extend_from_slice(b"de");

        let record = read_rpc_record(&mut buf.as_slice())
            .expect("record read")
            .expect("record present");
        assert_eq!(record, b"abcde");
    }

    #[test]
    fn record_size_cap_rejects_oversized() {
        // A single-fragment record whose declared length exceeds the cap
        // must be rejected before the server allocates that many bytes.
        let mut buf = Vec::new();
        let oversized = u32::try_from(MAX_RPC_RECORD_BYTES).expect("RPC cap fits u32") + 1;
        let marker = 0x8000_0000_u32 | oversized;
        buf.extend_from_slice(&marker.to_be_bytes());
        // Append just enough bytes to satisfy the first read_exact so the
        // size check fires rather than hitting UnexpectedEof.
        buf.resize(
            4 + usize::try_from(oversized).expect("oversized marker fits usize"),
            0,
        );
        let result = read_rpc_record(&mut buf.as_slice());
        assert!(result.is_err());
    }
}
