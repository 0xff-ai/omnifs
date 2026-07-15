mod support;

use omnifs_nfs::{
    Attr, DirListing, NFS4_OK, NodeKind, OpenRead, OpenResult, ReadOnlyExport, StateId, Status,
    StatusResult, start_server,
};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const RPC_CALL: u32 = 0;
const RPC_REPLY: u32 = 1;
const RPC_MSG_ACCEPTED: u32 = 0;
const RPC_SUCCESS: u32 = 0;
const AUTH_NONE: u32 = 0;
const NFS_PROGRAM: u32 = 100_003;
const NFS_VERSION_4: u32 = 4;
const PROC_COMPOUND: u32 = 1;
const NFS4ERR_DELAY: u32 = 10_008;

const CLAIM_NULL: u32 = 0;
const OP_CLOSE: u32 = 4;
const OP_GETFH: u32 = 10;
const OP_LOOKUP: u32 = 15;
const OP_OPEN: u32 = 18;
const OP_PUTFH: u32 = 22;
const OP_PUTROOTFH: u32 = 24;
const OP_READ: u32 = 25;
const OP_READDIR: u32 = 26;
const OP_SETCLIENTID: u32 = 35;
const OP_SETCLIENTID_CONFIRM: u32 = 36;

const LARGE_RANGED_TAIL_OFFSET: u64 = 64 * 1024 * 1024;

#[test]
fn nfs_tcp_server_lists_reads_and_closes_through_runtime() {
    let harness = support::test_export();
    let export: Arc<dyn ReadOnlyExport> = harness.export.clone();
    let trace_dir = tempfile::tempdir().expect("trace dir");
    let trace_path = trace_dir.path().join("server.trace");
    let server = start_server(
        export,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        Some(trace_path.clone()),
    )
    .expect("start loopback NFS server");
    let mut client = NfsTcpClient::connect(server.addr(), trace_path.clone());

    let volume_names = client.readdir(&["omnifs"]);
    assert!(volume_names.contains(&"test".to_string()));
    assert!(!volume_names.contains(&"omnifs".to_string()));

    let test_names_from_export_root = client.readdir(&["omnifs", "test"]);
    assert!(test_names_from_export_root.contains(&"hello".to_string()));
    assert!(test_names_from_export_root.contains(&"dynamic".to_string()));

    let test_names = client.readdir(&["test"]);
    assert!(test_names.contains(&"hello".to_string()));
    assert!(test_names.contains(&"dynamic".to_string()));

    let hello_names = client.readdir(&["test", "hello"]);
    assert!(hello_names.contains(&"message".to_string()));
    assert!(hello_names.contains(&"large-ranged".to_string()));

    let bundle_names = client.readdir(&["test", "hello", "bundle"]);
    assert!(bundle_names.contains(&"title".to_string()));
    assert!(bundle_names.contains(&"body".to_string()));

    let message = client.open_path(&["test", "hello"], "message");
    let (data, eof) = client.read(&message, 0, 64);
    assert_eq!(data, b"Hello, world!".to_vec());
    assert!(eof);
    client.close(&message);

    let dynamic = client.open_path(&["test", "dynamic", "alpha"], "value");
    let (data, eof) = client.read(&dynamic, 0, 64);
    assert_eq!(data, b"alpha\n".to_vec());
    assert!(eof);
    client.close(&dynamic);

    let large = client.open_path(&["test", "hello"], "large-ranged");
    let (data, eof) = client.read(&large, LARGE_RANGED_TAIL_OFFSET, 8);
    assert_eq!(data, b"L".to_vec());
    assert!(eof);
    client.close(&large);
}

#[test]
fn nfs_tcp_server_dispatches_rpcs_concurrently() {
    // A slow READDIR must not head-of-line block a later RPC on the same
    // connection: the fast GETFH reply has to come back while the READDIR is
    // still parked in the provider. A serial read/handle/write loop would be
    // stuck inside the parked READDIR and never reach the GETFH, so this test
    // would time out instead of passing.
    let gate = Gate::default();
    let export: Arc<dyn ReadOnlyExport> = Arc::new(GateExport { gate: gate.clone() });
    let server = start_server(
        export,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        None,
    )
    .expect("start loopback NFS server");

    let mut stream = TcpStream::connect(server.addr()).expect("connect NFS server");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .expect("set write timeout");

    let slow_xid = 9001;
    write_rpc_record(
        &mut stream,
        &rpc_call(
            slow_xid,
            &compound_payload(&[op_only(OP_PUTROOTFH), op_readdir()]),
        ),
    );

    // Block until the slow handler is parked inside the gated READDIR, then race
    // a fast GETFH on the same connection.
    Gate::wait(&gate.entered);
    let fast_xid = 9002;
    write_rpc_record(
        &mut stream,
        &rpc_call(
            fast_xid,
            &compound_payload(&[op_only(OP_PUTROOTFH), op_only(OP_GETFH)]),
        ),
    );

    let first = read_reply_xid(&mut stream);
    assert_eq!(
        first, fast_xid,
        "fast GETFH reply must return while the slow READDIR is still parked"
    );

    // Release the parked handler; its reply is still framed back afterwards.
    Gate::signal(&gate.released);
    let second = read_reply_xid(&mut stream);
    assert_eq!(
        second, slow_xid,
        "the released READDIR reply is delivered after the fast reply"
    );
}

/// Two one-shot condition flags used to rendezvous the test thread with a
/// handler thread parked inside the gated export.
#[derive(Clone, Default)]
struct Gate {
    entered: Arc<(Mutex<bool>, Condvar)>,
    released: Arc<(Mutex<bool>, Condvar)>,
}

impl Gate {
    fn signal(slot: &(Mutex<bool>, Condvar)) {
        *slot.0.lock().expect("gate lock") = true;
        slot.1.notify_all();
    }

    fn wait(slot: &(Mutex<bool>, Condvar)) {
        let mut flag = slot.0.lock().expect("gate lock");
        while !*flag {
            flag = slot.1.wait(flag).expect("gate wait");
        }
    }
}

/// Export whose root `readdir` parks on a gate while every other operation
/// returns immediately, so a slow READDIR and a fast GETFH can be raced over a
/// single connection.
struct GateExport {
    gate: Gate,
}

impl ReadOnlyExport for GateExport {
    fn generation(&self) -> u64 {
        1
    }

    fn set_clientid(&self, _verifier: [u8; 8], _owner: Vec<u8>) -> (u64, [u8; 8]) {
        (0, [0; 8])
    }

    fn confirm_client(&self, _clientid: u64, _verifier: &[u8]) -> StatusResult<()> {
        Err(Status::StaleClientId)
    }

    fn client_confirmed(&self, _clientid: u64) -> bool {
        false
    }

    fn root(&self) -> u64 {
        1
    }

    fn attr(&self, id: u64) -> StatusResult<Attr> {
        Ok(Attr {
            id,
            parent: 1,
            kind: NodeKind::Directory,
            size: 0,
            mode: 0o555,
            change: 1,
            mtime_sec: 0,
        })
    }

    fn lookup(&self, _parent: u64, _name: &str) -> StatusResult<u64> {
        Err(Status::NoEnt)
    }

    fn readdir(&self, _id: u64) -> StatusResult<DirListing> {
        Gate::signal(&self.gate.entered);
        Gate::wait(&self.gate.released);
        Ok(DirListing {
            entries: Vec::new(),
            exhaustive: true,
        })
    }

    fn read(&self, _id: u64) -> StatusResult<Vec<u8>> {
        Err(Status::Invalid)
    }

    fn readlink(&self, _id: u64) -> StatusResult<Vec<u8>> {
        Err(Status::Invalid)
    }

    fn open_state(&self, _id: u64, _clientid: u64, _access: u32) -> StatusResult<OpenResult> {
        Err(Status::Invalid)
    }

    fn validate_state(&self, _stateid: StateId) -> StatusResult<()> {
        Err(Status::BadStateId)
    }

    fn read_state(&self, _stateid: StateId, _offset: u64, _count: u32) -> StatusResult<OpenRead> {
        Err(Status::BadStateId)
    }

    fn close_state(&self, stateid: StateId) -> StatusResult<StateId> {
        Ok(stateid)
    }

    fn renew_client(&self, _clientid: u64) -> StatusResult<()> {
        Ok(())
    }
}

fn read_reply_xid(stream: &mut TcpStream) -> u32 {
    let mut header = [0; 4];
    stream.read_exact(&mut header).expect("read RPC marker");
    let marker = u32::from_be_bytes(header);
    assert_ne!(marker & 0x8000_0000, 0, "test expects one-fragment replies");
    let len = usize::try_from(marker & 0x7fff_ffff).expect("record length fits usize");
    let mut payload = vec![0; len];
    stream
        .read_exact(&mut payload)
        .expect("read RPC response payload");
    u32::from_be_bytes(payload[..4].try_into().expect("xid is u32"))
}

struct NfsTcpClient {
    stream: TcpStream,
    xid: u32,
    clientid: u64,
    trace_path: PathBuf,
}

impl NfsTcpClient {
    fn connect(addr: SocketAddr, trace_path: PathBuf) -> Self {
        let stream = TcpStream::connect(addr).expect("connect NFS server");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set read timeout");
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .expect("set write timeout");
        let mut client = Self {
            stream,
            xid: 1,
            clientid: 0,
            trace_path,
        };
        client.clientid = client.set_clientid();
        client
    }

    fn set_clientid(&mut self) -> u64 {
        let body = self.compound(&[op_setclientid()]);
        let mut reader = XdrReader::new(&body);
        assert_compound_header(&mut reader, 1);
        assert_op_status(&mut reader, OP_SETCLIENTID);
        let clientid = reader.u64();
        let verifier = reader.fixed_opaque(8);

        let body = self.compound(&[op_setclientid_confirm(clientid, &verifier)]);
        let mut reader = XdrReader::new(&body);
        assert_compound_header(&mut reader, 1);
        assert_op_status(&mut reader, OP_SETCLIENTID_CONFIRM);
        clientid
    }

    fn readdir(&mut self, path: &[&str]) -> Vec<String> {
        let mut ops = path_ops(path);
        ops.push(op_readdir());
        let deadline = Instant::now() + Duration::from_secs(10);
        let body = loop {
            let (xid, body) = self.compound_reply(&ops);
            let top_status = u32::from_be_bytes(
                body.get(..4)
                    .expect("compound response contains a status")
                    .try_into()
                    .expect("status is u32"),
            );
            if top_status == NFS4ERR_DELAY && Instant::now() < deadline {
                continue;
            }
            if top_status != NFS4_OK {
                let trace =
                    std::fs::read_to_string(&self.trace_path).unwrap_or_else(|_| String::new());
                panic!("READDIR compound xid={xid} failed with status {top_status}\n{trace}");
            }
            break body;
        };
        let mut reader = XdrReader::new(&body);
        assert_compound_header(&mut reader, ops.len());
        for op in ops.iter().take(ops.len() - 1) {
            assert_op_status(&mut reader, first_u32(op));
        }
        assert_op_status(&mut reader, OP_READDIR);
        let _verifier = reader.fixed_opaque(8);
        let mut names = Vec::new();
        while reader.bool() {
            let _cookie = reader.u64();
            names.push(reader.string());
            reader.fattr();
        }
        assert!(
            reader.bool(),
            "READDIR should report EOF for the finite known provider snapshot"
        );
        names
    }

    fn open_path(&mut self, parent: &[&str], name: &str) -> OpenedFile {
        let mut ops = path_ops(parent);
        ops.push(op_open(name, self.clientid));
        ops.push(op_only(OP_GETFH));
        let body = self.compound(&ops);
        let mut reader = XdrReader::new(&body);
        assert_compound_header(&mut reader, ops.len());
        for op in ops.iter().take(ops.len() - 2) {
            assert_op_status(&mut reader, first_u32(op));
        }

        assert_op_status(&mut reader, OP_OPEN);
        let stateid = reader
            .fixed_opaque(16)
            .try_into()
            .expect("stateid is 16 bytes");
        let _atomic = reader.bool();
        let _before = reader.u64();
        let _after = reader.u64();
        let _rflags = reader.u32();
        reader.bitmap();
        let _delegate = reader.u32();

        assert_op_status(&mut reader, OP_GETFH);
        let filehandle = reader.opaque();
        OpenedFile {
            stateid,
            filehandle,
        }
    }

    fn read(&mut self, file: &OpenedFile, offset: u64, count: u32) -> (Vec<u8>, bool) {
        let ops = [
            op_putfh(&file.filehandle),
            op_read(file.stateid, offset, count),
        ];
        let body = self.compound(&ops);
        let mut reader = XdrReader::new(&body);
        assert_compound_header(&mut reader, 2);
        assert_op_status(&mut reader, OP_PUTFH);
        assert_op_status(&mut reader, OP_READ);
        let eof = reader.bool();
        let data = reader.opaque();
        (data, eof)
    }

    fn close(&mut self, file: &OpenedFile) {
        let ops = [op_putfh(&file.filehandle), op_close(file.stateid)];
        let body = self.compound(&ops);
        let mut reader = XdrReader::new(&body);
        assert_compound_header(&mut reader, 2);
        assert_op_status(&mut reader, OP_PUTFH);
        assert_op_status(&mut reader, OP_CLOSE);
        let _next_stateid = reader.fixed_opaque(16);
    }

    fn compound(&mut self, ops: &[Vec<u8>]) -> Vec<u8> {
        let (xid, body) = self.compound_reply(ops);
        if body.len() >= 4 {
            let top_status = u32::from_be_bytes(body[..4].try_into().expect("status is u32"));
            if top_status != NFS4_OK {
                let trace =
                    std::fs::read_to_string(&self.trace_path).unwrap_or_else(|_| String::new());
                panic!("compound xid={xid} failed with status {top_status}\n{trace}");
            }
        }
        body
    }

    fn compound_reply(&mut self, ops: &[Vec<u8>]) -> (u32, Vec<u8>) {
        let xid = self.next_xid();
        write_rpc_record(&mut self.stream, &rpc_call(xid, &compound_payload(ops)));
        let body = read_rpc_success(&mut self.stream, xid);
        (xid, body)
    }

    fn next_xid(&mut self) -> u32 {
        let xid = self.xid;
        self.xid = self.xid.checked_add(1).expect("test xid overflow");
        xid
    }
}

struct OpenedFile {
    stateid: [u8; 16],
    filehandle: Vec<u8>,
}

fn path_ops(path: &[&str]) -> Vec<Vec<u8>> {
    let mut ops = vec![op_only(OP_PUTROOTFH)];
    for name in path {
        ops.push(op_lookup(name));
    }
    ops
}

fn op_only(op: u32) -> Vec<u8> {
    let mut writer = XdrWriter::new();
    writer.u32(op);
    writer.into_inner()
}

fn op_lookup(name: &str) -> Vec<u8> {
    let mut writer = XdrWriter::new();
    writer.u32(OP_LOOKUP);
    writer.string(name);
    writer.into_inner()
}

fn op_putfh(filehandle: &[u8]) -> Vec<u8> {
    let mut writer = XdrWriter::new();
    writer.u32(OP_PUTFH);
    writer.opaque(filehandle);
    writer.into_inner()
}

fn op_readdir() -> Vec<u8> {
    let mut writer = XdrWriter::new();
    writer.u32(OP_READDIR);
    writer.u64(0);
    writer.fixed_opaque(&[0; 8]);
    writer.u32(4096);
    writer.u32(4096);
    writer.bitmap(&[]);
    writer.into_inner()
}

fn op_open(name: &str, clientid: u64) -> Vec<u8> {
    let mut writer = XdrWriter::new();
    writer.u32(OP_OPEN);
    writer.u32(1);
    writer.u32(1);
    writer.u32(0);
    writer.u64(clientid);
    writer.opaque(b"socket-owner");
    writer.u32(0);
    writer.u32(CLAIM_NULL);
    writer.string(name);
    writer.into_inner()
}

fn op_read(stateid: [u8; 16], offset: u64, count: u32) -> Vec<u8> {
    let mut writer = XdrWriter::new();
    writer.u32(OP_READ);
    writer.fixed_opaque(&stateid);
    writer.u64(offset);
    writer.u32(count);
    writer.into_inner()
}

fn op_close(stateid: [u8; 16]) -> Vec<u8> {
    let mut writer = XdrWriter::new();
    writer.u32(OP_CLOSE);
    writer.u32(1);
    writer.fixed_opaque(&stateid);
    writer.into_inner()
}

fn op_setclientid() -> Vec<u8> {
    let mut writer = XdrWriter::new();
    writer.u32(OP_SETCLIENTID);
    writer.fixed_opaque(&[0; 8]);
    writer.opaque(b"omnifs-nfs-socket-test");
    writer.u32(0);
    writer.string("");
    writer.string("");
    writer.u32(0);
    writer.into_inner()
}

fn op_setclientid_confirm(clientid: u64, verifier: &[u8]) -> Vec<u8> {
    let mut writer = XdrWriter::new();
    writer.u32(OP_SETCLIENTID_CONFIRM);
    writer.u64(clientid);
    writer.fixed_opaque(verifier);
    writer.into_inner()
}

fn compound_payload(ops: &[Vec<u8>]) -> Vec<u8> {
    let mut writer = XdrWriter::new();
    writer.string("socket");
    writer.u32(0);
    writer.u32(u32::try_from(ops.len()).expect("op count fits u32"));
    for op in ops {
        writer.fixed_opaque(op);
    }
    writer.into_inner()
}

fn rpc_call(xid: u32, body: &[u8]) -> Vec<u8> {
    let mut writer = XdrWriter::new();
    writer.u32(xid);
    writer.u32(RPC_CALL);
    writer.u32(2);
    writer.u32(NFS_PROGRAM);
    writer.u32(NFS_VERSION_4);
    writer.u32(PROC_COMPOUND);
    writer.u32(AUTH_NONE);
    writer.u32(0);
    writer.u32(AUTH_NONE);
    writer.u32(0);
    writer.fixed_opaque(body);
    writer.into_inner()
}

fn write_rpc_record(stream: &mut TcpStream, payload: &[u8]) {
    let len = u32::try_from(payload.len()).expect("RPC payload fits u32");
    assert!(len <= 0x7fff_ffff);
    stream
        .write_all(&(0x8000_0000 | len).to_be_bytes())
        .expect("write RPC marker");
    stream.write_all(payload).expect("write RPC payload");
    stream.flush().expect("flush RPC payload");
}

fn read_rpc_success(stream: &mut TcpStream, xid: u32) -> Vec<u8> {
    let mut header = [0; 4];
    stream.read_exact(&mut header).expect("read RPC marker");
    let marker = u32::from_be_bytes(header);
    assert_ne!(marker & 0x8000_0000, 0, "test expects one-fragment replies");
    let len = usize::try_from(marker & 0x7fff_ffff).expect("record length fits usize");
    let mut payload = vec![0; len];
    stream
        .read_exact(&mut payload)
        .expect("read RPC response payload");

    let mut reader = XdrReader::new(&payload);
    assert_eq!(reader.u32(), xid);
    assert_eq!(reader.u32(), RPC_REPLY);
    assert_eq!(reader.u32(), RPC_MSG_ACCEPTED);
    assert_eq!(reader.u32(), AUTH_NONE);
    assert!(reader.opaque().is_empty());
    assert_eq!(reader.u32(), RPC_SUCCESS);
    reader.remaining().to_vec()
}

fn assert_compound_header(reader: &mut XdrReader<'_>, op_count: usize) {
    assert_eq!(reader.u32(), NFS4_OK);
    assert_eq!(reader.string(), "socket");
    assert_eq!(
        reader.u32(),
        u32::try_from(op_count).expect("op count fits u32")
    );
}

fn assert_op_status(reader: &mut XdrReader<'_>, op: u32) {
    assert_eq!(reader.u32(), op);
    assert_eq!(reader.u32(), NFS4_OK, "op {op} should succeed");
}

fn first_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes[..4].try_into().expect("op starts with u32"))
}

struct XdrWriter {
    out: Vec<u8>,
}

impl XdrWriter {
    fn new() -> Self {
        Self { out: Vec::new() }
    }

    fn into_inner(self) -> Vec<u8> {
        self.out
    }

    fn fixed_opaque(&mut self, bytes: &[u8]) {
        self.out.extend_from_slice(bytes);
        let pad = (4 - (bytes.len() % 4)) % 4;
        self.out.resize(self.out.len() + pad, 0);
    }

    fn u32(&mut self, value: u32) {
        self.out.extend_from_slice(&value.to_be_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.out.extend_from_slice(&value.to_be_bytes());
    }

    fn string(&mut self, value: &str) {
        self.opaque(value.as_bytes());
    }

    fn opaque(&mut self, bytes: &[u8]) {
        self.u32(u32::try_from(bytes.len()).expect("opaque length fits u32"));
        self.fixed_opaque(bytes);
    }

    fn bitmap(&mut self, bits: &[u32]) {
        self.u32(u32::try_from(bits.len()).expect("bitmap word count fits u32"));
        for bit in bits {
            self.u32(*bit);
        }
    }
}

struct XdrReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> XdrReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn remaining(&self) -> &'a [u8] {
        &self.bytes[self.pos..]
    }

    fn bool(&mut self) -> bool {
        self.u32() != 0
    }

    fn u32(&mut self) -> u32 {
        u32::from_be_bytes(self.take(4).try_into().expect("u32 has 4 bytes"))
    }

    fn u64(&mut self) -> u64 {
        u64::from_be_bytes(self.take(8).try_into().expect("u64 has 8 bytes"))
    }

    fn fixed_opaque(&mut self, len: usize) -> Vec<u8> {
        let bytes = self.take(len).to_vec();
        self.skip_padding(len);
        bytes
    }

    fn opaque(&mut self) -> Vec<u8> {
        let len = usize::try_from(self.u32()).expect("opaque length fits usize");
        self.fixed_opaque(len)
    }

    fn string(&mut self) -> String {
        String::from_utf8(self.opaque()).expect("valid XDR string")
    }

    fn bitmap(&mut self) -> Vec<u32> {
        let len = usize::try_from(self.u32()).expect("bitmap length fits usize");
        (0..len).map(|_| self.u32()).collect()
    }

    fn fattr(&mut self) {
        let _bitmap = self.bitmap();
        let _values = self.opaque();
    }

    fn take(&mut self, len: usize) -> &'a [u8] {
        let end = self.pos.checked_add(len).expect("XDR offset overflow");
        assert!(end <= self.bytes.len(), "XDR underflow");
        let start = self.pos;
        self.pos = end;
        &self.bytes[start..end]
    }

    fn skip_padding(&mut self, len: usize) {
        let pad = (4 - (len % 4)) % 4;
        self.take(pad);
    }
}
