use crate::export::{
    Attr, DirEntry, DirListing, NodeKind, OpenRead, OpenResult, OpenSeed, OpenTable,
    ReadOnlyExport, StateId, Status, StatusResult,
};
use crate::protocol::attrs::encode_bitmap;
use crate::protocol::client::ClientTable;
use crate::protocol::compound::handle_compound;
use crate::protocol::consts::{
    CLAIM_NULL, FATTR4_FILEID, FATTR4_SIZE, FATTR4_TYPE, NF4REG, NFS4_OK, NFS4ERR_BAD_COOKIE,
    NFS4ERR_DELAY, NFS4ERR_FHEXPIRED, NFS4ERR_INVAL, NFS4ERR_MINOR_VERS_MISMATCH, NFS4ERR_NOENT,
    NFS4ERR_NOFILEHANDLE, NFS4ERR_NOTDIR, NFS4ERR_NOTSUPP, NFS4ERR_OP_ILLEGAL,
    NFS4ERR_STALE_CLIENTID, NFS4ERR_SYMLINK, NFS4ERR_TOOSMALL, OP_CLOSE, OP_GETATTR, OP_GETFH,
    OP_ILLEGAL, OP_LOOKUP, OP_OPEN, OP_OPEN_CONFIRM, OP_PUTFH, OP_PUTROOTFH, OP_READ, OP_READDIR,
    OP_READLINK, OP_SECINFO,
};
use crate::protocol::filehandle::{client_id, decode_file_handle, file_handle, now_sec};
use crate::protocol::ops::handle_readdir;
use crate::protocol::xdr::{XdrError, XdrReader, XdrWriter};
use crate::trace::Trace;
use std::collections::BTreeMap;

const TEST_GENERATION: u64 = 7;

#[derive(Clone)]
struct StaticNode {
    attr: Attr,
    name: String,
    data: StaticData,
}

#[derive(Clone)]
enum StaticData {
    Dir(Vec<u64>),
    File(Vec<u8>),
    Symlink(Vec<u8>),
}

struct StaticExport {
    nodes: BTreeMap<u64, StaticNode>,
    opens: OpenTable<Vec<u8>>,
    exhaustive_listings: bool,
}

impl StaticExport {
    fn fixture() -> Self {
        let now = now_sec();
        let mut nodes = BTreeMap::new();
        nodes.insert(
            1,
            StaticNode {
                name: String::new(),
                attr: Attr {
                    id: 1,
                    parent: 1,
                    kind: NodeKind::Directory,
                    size: 0,
                    mode: 0o555,
                    change: 1,
                    mtime_sec: now,
                },
                data: StaticData::Dir(vec![2, 3]),
            },
        );
        nodes.insert(
            2,
            StaticNode {
                name: "README.txt".to_string(),
                attr: Attr {
                    id: 2,
                    parent: 1,
                    kind: NodeKind::File,
                    size: 12,
                    mode: 0o444,
                    change: 2,
                    mtime_sec: now,
                },
                data: StaticData::File(b"hello nfs v4".to_vec()),
            },
        );
        nodes.insert(
            3,
            StaticNode {
                name: "readme-link".to_string(),
                attr: Attr {
                    id: 3,
                    parent: 1,
                    kind: NodeKind::Symlink,
                    size: 10,
                    mode: 0o777,
                    change: 3,
                    mtime_sec: now,
                },
                data: StaticData::Symlink(b"README.txt".to_vec()),
            },
        );
        Self {
            nodes,
            opens: OpenTable::new(),
            exhaustive_listings: true,
        }
    }

    fn fixture_with_root_children(children: Vec<u64>) -> Self {
        let mut fixture = Self::fixture();
        let root = fixture.nodes.get_mut(&1).expect("root node");
        root.data = StaticData::Dir(children);
        fixture
    }

    fn fixture_non_exhaustive() -> Self {
        let mut fixture = Self::fixture();
        fixture.exhaustive_listings = false;
        fixture
    }
}

impl ReadOnlyExport for StaticExport {
    fn root(&self) -> u64 {
        1
    }

    fn attr(&self, id: u64) -> StatusResult<Attr> {
        self.nodes
            .get(&id)
            .map(|node| node.attr.clone())
            .ok_or(Status::Stale)
    }

    fn lookup(&self, parent: u64, name: &str) -> StatusResult<u64> {
        match name {
            "." => return Ok(parent),
            ".." => return self.parent(parent),
            _ => {},
        }
        let parent = self.nodes.get(&parent).ok_or(Status::Stale)?;
        let StaticData::Dir(children) = &parent.data else {
            return Err(Status::NotDir);
        };
        children
            .iter()
            .copied()
            .find(|id| self.nodes.get(id).is_some_and(|node| node.name == name))
            .ok_or(Status::NoEnt)
    }

    fn readdir(&self, id: u64) -> StatusResult<DirListing> {
        let node = self.nodes.get(&id).ok_or(Status::Stale)?;
        let StaticData::Dir(children) = &node.data else {
            return Err(Status::NotDir);
        };
        let entries = children
            .iter()
            .filter_map(|id| self.nodes.get(id))
            .map(|node| DirEntry {
                id: node.attr.id,
                name: node.name.clone(),
                attr: node.attr.clone(),
            })
            .collect();
        Ok(DirListing {
            entries,
            exhaustive: self.exhaustive_listings,
        })
    }

    fn read(&self, id: u64) -> StatusResult<Vec<u8>> {
        match &self.nodes.get(&id).ok_or(Status::Stale)?.data {
            StaticData::File(data) => Ok(data.clone()),
            StaticData::Dir(_) => Err(Status::IsDir),
            StaticData::Symlink(_) => Err(Status::Invalid),
        }
    }

    fn readlink(&self, id: u64) -> StatusResult<Vec<u8>> {
        match &self.nodes.get(&id).ok_or(Status::Stale)?.data {
            StaticData::Symlink(data) => Ok(data.clone()),
            _ => Err(Status::Invalid),
        }
    }

    fn open_state(
        &self,
        generation: u64,
        id: u64,
        clientid: u64,
        access: u32,
    ) -> StatusResult<OpenResult> {
        let data = self.read(id)?;
        let attr = self.attr(id)?;
        let stateid = self.opens.open(OpenSeed {
            generation,
            inode: id,
            clientid,
            access,
            body: data,
        });
        Ok(OpenResult { stateid, attr })
    }

    fn validate_state(&self, stateid: StateId) -> StatusResult<()> {
        self.opens.touch(stateid)
    }

    fn read_state(&self, stateid: StateId, offset: u64, count: u32) -> StatusResult<OpenRead> {
        self.opens.read(stateid, offset, count)
    }

    fn close_state(&self, stateid: StateId) -> StatusResult<StateId> {
        self.opens.close(stateid).map(|(next, _)| next)
    }

    fn renew_client(&self, clientid: u64) -> StatusResult<()> {
        self.opens.renew_client(clientid);
        Ok(())
    }
}

fn compound_payload(ops: &[Vec<u8>]) -> Vec<u8> {
    let mut writer = XdrWriter::new();
    writer.string("test");
    writer.u32(0);
    writer.u32(u32::try_from(ops.len()).expect("compound op count fits u32"));
    for op in ops {
        writer.bytes(op);
    }
    writer.into_inner()
}

fn op_only(op: u32) -> Vec<u8> {
    let mut writer = XdrWriter::new();
    writer.u32(op);
    writer.into_inner()
}

fn op_getattr(bits: &[u32]) -> Vec<u8> {
    let mut writer = XdrWriter::new();
    writer.u32(OP_GETATTR);
    encode_bitmap(&mut writer, bits);
    writer.into_inner()
}

fn request_words(bits: &[u32]) -> Vec<u32> {
    let mut writer = XdrWriter::new();
    encode_bitmap(&mut writer, bits);
    let bytes = writer.into_inner();
    XdrReader::new(&bytes).bitmap().unwrap()
}

fn op_lookup(name: &str) -> Vec<u8> {
    let mut writer = XdrWriter::new();
    writer.u32(OP_LOOKUP);
    writer.string(name);
    writer.into_inner()
}

fn op_open(name: &str, share_access: u32) -> Vec<u8> {
    op_open_with_deny(name, share_access, 0)
}

fn op_open_with_deny(name: &str, share_access: u32, share_deny: u32) -> Vec<u8> {
    let mut writer = XdrWriter::new();
    writer.u32(OP_OPEN);
    writer.u32(1);
    writer.u32(share_access);
    writer.u32(share_deny);
    writer.u64(client_id(TEST_GENERATION));
    writer.opaque(b"owner");
    writer.u32(0);
    writer.u32(CLAIM_NULL);
    writer.string(name);
    writer.into_inner()
}

fn op_read(stateid: [u8; 16], offset: u64, count: u32) -> Vec<u8> {
    let mut writer = XdrWriter::new();
    writer.u32(OP_READ);
    writer.bytes(&stateid);
    writer.u64(offset);
    writer.u32(count);
    writer.into_inner()
}

fn op_close(stateid: [u8; 16]) -> Vec<u8> {
    let mut writer = XdrWriter::new();
    writer.u32(OP_CLOSE);
    writer.u32(1);
    writer.bytes(&stateid);
    writer.into_inner()
}

fn op_readdir() -> Vec<u8> {
    op_readdir_with_maxcount(4096)
}

fn op_readdir_with_maxcount(maxcount: u32) -> Vec<u8> {
    let mut writer = XdrWriter::new();
    writer.u32(OP_READDIR);
    writer.u64(0);
    writer.bytes(&[0; 8]);
    writer.u32(4096);
    writer.u32(maxcount);
    encode_bitmap(&mut writer, &[FATTR4_TYPE, FATTR4_SIZE, FATTR4_FILEID]);
    writer.into_inner()
}

fn op_secinfo(name: &str) -> Vec<u8> {
    let mut writer = XdrWriter::new();
    writer.u32(OP_SECINFO);
    writer.string(name);
    writer.into_inner()
}

#[derive(Debug, PartialEq, Eq)]
struct ReaddirResult {
    verifier: [u8; 8],
    entries: Vec<(u64, String)>,
    eof: bool,
}

fn decode_readdir_result(result: &[u8]) -> ReaddirResult {
    let mut reader = XdrReader::new(result);
    assert_eq!(reader.u32().unwrap(), OP_READDIR);
    assert_eq!(reader.u32().unwrap(), NFS4_OK);
    let verifier = reader
        .fixed_opaque(8)
        .unwrap()
        .try_into()
        .expect("READDIR verifier is fixed length");
    let mut entries = Vec::new();
    while reader.u32().unwrap() != 0 {
        let cookie = reader.u64().unwrap();
        let name = reader.string().unwrap();
        let _attrs = reader.fattr().unwrap();
        entries.push((cookie, name));
    }
    let eof = reader.u32().unwrap() != 0;
    ReaddirResult {
        verifier,
        entries,
        eof,
    }
}

fn compound_status(export: &dyn ReadOnlyExport, ops: &[Vec<u8>]) -> u32 {
    compound_status_with_clients(
        export,
        &ClientTable::with_confirmed_default(TEST_GENERATION),
        ops,
    )
}

fn compound_status_with_clients(
    export: &dyn ReadOnlyExport,
    clients: &ClientTable,
    ops: &[Vec<u8>],
) -> u32 {
    let payload = compound_payload(ops);
    let mut reader = XdrReader::new(&payload);
    let result = handle_compound(
        &mut reader,
        TEST_GENERATION,
        clients,
        export,
        1,
        &Trace::new(None).unwrap(),
    )
    .unwrap();
    let mut reader = XdrReader::new(&result);
    reader.u32().unwrap()
}

fn compound_result(export: &dyn ReadOnlyExport, ops: &[Vec<u8>]) -> Vec<u8> {
    compound_result_with_minor(export, 0, ops)
}

fn compound_result_with_minor(export: &dyn ReadOnlyExport, minor: u32, ops: &[Vec<u8>]) -> Vec<u8> {
    let mut payload = compound_payload(ops);
    payload[8..12].copy_from_slice(&minor.to_be_bytes());
    let mut reader = XdrReader::new(&payload);
    let clients = ClientTable::with_confirmed_default(TEST_GENERATION);
    handle_compound(
        &mut reader,
        TEST_GENERATION,
        &clients,
        export,
        1,
        &Trace::new(None).unwrap(),
    )
    .unwrap()
}

#[test]
fn open_create_fattr_with_malformed_bitmap_returns_decode_error() {
    let export = StaticExport::fixture();
    let mut op = XdrWriter::new();
    op.u32(OP_OPEN);
    op.u32(1);
    op.u32(1);
    op.u32(0);
    op.u64(client_id(TEST_GENERATION));
    op.opaque(b"owner");
    op.u32(1);
    op.u32(0);
    op.u32(u32::MAX);
    let payload = compound_payload(&[op.into_inner()]);
    let mut reader = XdrReader::new(&payload);
    let clients = ClientTable::with_confirmed_default(TEST_GENERATION);

    let error = handle_compound(
        &mut reader,
        TEST_GENERATION,
        &clients,
        &export,
        1,
        &Trace::new(None).unwrap(),
    )
    .expect_err("malformed OPEN create fattr should preserve its XDR decode error");

    assert!(matches!(error, XdrError::Underflow));
}

fn open_stateid(result: &[u8]) -> [u8; 16] {
    let mut reader = XdrReader::new(result);
    assert_eq!(reader.u32().unwrap(), NFS4_OK);
    assert_eq!(reader.string().unwrap(), "test");
    assert_eq!(reader.u32().unwrap(), 2);
    assert_eq!(reader.u32().unwrap(), OP_PUTROOTFH);
    assert_eq!(reader.u32().unwrap(), NFS4_OK);
    assert_eq!(reader.u32().unwrap(), OP_OPEN);
    assert_eq!(reader.u32().unwrap(), NFS4_OK);
    let raw = reader.fixed_opaque(16).unwrap();
    raw.try_into().expect("OPEN stateid has fixed length")
}

fn op_putfh(id: u64) -> Vec<u8> {
    let mut writer = XdrWriter::new();
    writer.u32(OP_PUTFH);
    writer.opaque(&file_handle(7, id));
    writer.into_inner()
}

#[test]
fn synthetic_protocol_edge_cases() {
    let export = StaticExport::fixture();

    let open = compound_result(&export, &[op_only(OP_PUTROOTFH), op_open("README.txt", 1)]);
    let stateid = open_stateid(&open);
    let status = compound_status(
        &export,
        &[op_putfh(2), op_read(stateid, 0, 64), op_close(stateid)],
    );
    assert_eq!(status, NFS4_OK);

    let (status, _result) = handle_readdir(
        &export,
        7,
        Some(export.root()),
        0,
        &[0xcc; 8],
        4096,
        &[FATTR4_TYPE, FATTR4_SIZE, FATTR4_FILEID],
    );
    assert_eq!(status, NFS4_OK);

    let status = compound_status(
        &export,
        &[op_only(OP_PUTROOTFH), op_lookup("README.txt"), op_readdir()],
    );
    assert_eq!(status, NFS4ERR_NOTDIR);

    let (status, _result) = handle_readdir(
        &export,
        7,
        Some(export.root()),
        5,
        &[0xbb; 8],
        4096,
        &[FATTR4_TYPE, FATTR4_SIZE, FATTR4_FILEID],
    );
    assert_eq!(status, NFS4ERR_BAD_COOKIE);
}

#[test]
fn synthetic_readonly_compound_ops_succeed() {
    let export = StaticExport::fixture();
    assert_eq!(
        compound_status(&export, &[op_only(OP_PUTROOTFH), op_readdir()]),
        NFS4_OK
    );
    assert_eq!(
        compound_status(
            &export,
            &[
                op_only(OP_PUTROOTFH),
                op_lookup("readme-link"),
                op_only(OP_READLINK),
            ],
        ),
        NFS4_OK
    );
}

#[test]
fn synthetic_opens_create_independent_stateids() {
    let export = StaticExport::fixture();
    let first = compound_result(&export, &[op_only(OP_PUTROOTFH), op_open("README.txt", 1)]);
    let second = compound_result(&export, &[op_only(OP_PUTROOTFH), op_open("README.txt", 1)]);
    let first_stateid = open_stateid(&first);
    let second_stateid = open_stateid(&second);
    assert_ne!(first_stateid, second_stateid);

    let close_first = compound_status(&export, &[op_putfh(2), op_close(first_stateid)]);
    assert_eq!(close_first, NFS4_OK);
    let read_second = compound_status(&export, &[op_putfh(2), op_read(second_stateid, 0, 5)]);
    assert_eq!(read_second, NFS4_OK);
}

#[test]
fn synthetic_lookup_rejects_invalid_components() {
    let export = StaticExport::fixture();
    for name in [
        "",
        ".",
        "..",
        "a/b",
        "../escape",
        "/escape",
        r"a\b",
        "bad\0name",
    ] {
        let status = compound_status(&export, &[op_only(OP_PUTROOTFH), op_lookup(name)]);
        assert_eq!(status, NFS4ERR_INVAL, "name={name:?}");
    }
}

#[test]
fn synthetic_readdir_respects_maxcount() {
    let export = StaticExport::fixture();
    let (status, result) = handle_readdir(
        &export,
        7,
        Some(export.root()),
        0,
        &[0; 8],
        96,
        &[FATTR4_TYPE, FATTR4_SIZE, FATTR4_FILEID],
    );
    assert_eq!(status, NFS4_OK);
    assert!(result.len() - 8 <= 96);
    let decoded = decode_readdir_result(&result);
    assert_eq!(decoded.entries.len(), 1);
    assert!(!decoded.eof);
}

#[test]
fn synthetic_readdir_uses_snapshot_verifier_for_continuation() {
    let export = StaticExport::fixture();
    let (status, first) = handle_readdir(
        &export,
        7,
        Some(export.root()),
        0,
        &[0xaa; 8],
        96,
        &[FATTR4_TYPE, FATTR4_SIZE, FATTR4_FILEID],
    );
    assert_eq!(status, NFS4_OK);
    let first = decode_readdir_result(&first);
    assert_eq!(first.entries.len(), 1);
    assert_eq!(first.entries[0].1, "README.txt");

    let (status, second) = handle_readdir(
        &export,
        7,
        Some(export.root()),
        first.entries[0].0,
        &first.verifier,
        4096,
        &[FATTR4_TYPE, FATTR4_SIZE, FATTR4_FILEID],
    );
    assert_eq!(status, NFS4_OK);
    let second = decode_readdir_result(&second);
    assert_eq!(second.verifier, first.verifier);
    assert_eq!(second.entries, vec![(4, "readme-link".to_string())]);
    assert!(second.eof);
}

#[test]
fn synthetic_readdir_non_exhaustive_listing_returns_known_snapshot() {
    // NFS cannot express "these entries are known, but dynamic children may
    // also exist". Return the finite provider snapshot as a normal directory
    // listing so shell tools can still traverse known entries; explicit LOOKUP
    // remains responsible for named dynamic children.
    let export = StaticExport::fixture_non_exhaustive();
    let (status, result) = handle_readdir(
        &export,
        7,
        Some(export.root()),
        0,
        &[0; 8],
        4096,
        &[FATTR4_TYPE, FATTR4_SIZE, FATTR4_FILEID],
    );
    assert_eq!(status, NFS4_OK);
    let decoded = decode_readdir_result(&result);
    assert_eq!(
        decoded.entries,
        vec![
            (3, "README.txt".to_string()),
            (4, "readme-link".to_string())
        ]
    );
    assert!(decoded.eof);
}

#[test]
fn synthetic_readdir_can_return_delay() {
    struct DelayExport(StaticExport);

    impl ReadOnlyExport for DelayExport {
        fn root(&self) -> u64 {
            self.0.root()
        }

        fn attr(&self, id: u64) -> StatusResult<Attr> {
            self.0.attr(id)
        }

        fn lookup(&self, parent: u64, name: &str) -> StatusResult<u64> {
            self.0.lookup(parent, name)
        }

        fn readdir(&self, _id: u64) -> StatusResult<DirListing> {
            Err(Status::Delay)
        }

        fn read(&self, id: u64) -> StatusResult<Vec<u8>> {
            self.0.read(id)
        }

        fn readlink(&self, id: u64) -> StatusResult<Vec<u8>> {
            self.0.readlink(id)
        }

        fn open_state(
            &self,
            generation: u64,
            id: u64,
            clientid: u64,
            access: u32,
        ) -> StatusResult<OpenResult> {
            self.0.open_state(generation, id, clientid, access)
        }

        fn validate_state(&self, stateid: StateId) -> StatusResult<()> {
            self.0.validate_state(stateid)
        }

        fn read_state(&self, stateid: StateId, offset: u64, count: u32) -> StatusResult<OpenRead> {
            self.0.read_state(stateid, offset, count)
        }

        fn close_state(&self, stateid: StateId) -> StatusResult<StateId> {
            self.0.close_state(stateid)
        }

        fn renew_client(&self, clientid: u64) -> StatusResult<()> {
            self.0.renew_client(clientid)
        }
    }

    let export = DelayExport(StaticExport::fixture());
    assert_eq!(
        compound_status(&export, &[op_only(OP_PUTROOTFH), op_readdir()]),
        NFS4ERR_DELAY
    );
}

#[test]
fn synthetic_readdir_sorts_entries_before_assigning_cookies() {
    let export = StaticExport::fixture_with_root_children(vec![3, 2]);
    let (status, result) = handle_readdir(
        &export,
        7,
        Some(export.root()),
        0,
        &[0; 8],
        4096,
        &[FATTR4_TYPE, FATTR4_SIZE, FATTR4_FILEID],
    );
    assert_eq!(status, NFS4_OK);
    let decoded = decode_readdir_result(&result);
    assert_eq!(
        decoded.entries,
        vec![
            (3, "README.txt".to_string()),
            (4, "readme-link".to_string())
        ]
    );
}

#[test]
fn synthetic_readdir_maxcount_exact_boundary_includes_trailer() {
    let export = StaticExport::fixture_with_root_children(vec![2]);
    let (status, full) = handle_readdir(
        &export,
        7,
        Some(export.root()),
        0,
        &[0; 8],
        4096,
        &[FATTR4_TYPE, FATTR4_SIZE, FATTR4_FILEID],
    );
    assert_eq!(status, NFS4_OK);
    let maxcount = u32::try_from(full.len() - 8).expect("test READDIR result body fits u32");

    let (status, exact) = handle_readdir(
        &export,
        7,
        Some(export.root()),
        0,
        &[0; 8],
        maxcount,
        &[FATTR4_TYPE, FATTR4_SIZE, FATTR4_FILEID],
    );
    assert_eq!(status, NFS4_OK);
    assert_eq!(exact.len(), full.len());

    let (status, _too_small) = handle_readdir(
        &export,
        7,
        Some(export.root()),
        0,
        &[0; 8],
        maxcount - 1,
        &[FATTR4_TYPE, FATTR4_SIZE, FATTR4_FILEID],
    );
    assert_eq!(status, NFS4ERR_TOOSMALL);
}

#[test]
fn synthetic_open_rejections() {
    let export = StaticExport::fixture();
    for share_access in [0, 0x8000_0000] {
        let status = compound_status(
            &export,
            &[op_only(OP_PUTROOTFH), op_open("README.txt", share_access)],
        );
        assert_eq!(status, NFS4ERR_INVAL, "share_access={share_access}");
    }

    let clients = ClientTable::new(TEST_GENERATION);
    let status = compound_status_with_clients(
        &export,
        &clients,
        &[op_only(OP_PUTROOTFH), op_open("README.txt", 1)],
    );
    assert_eq!(status, NFS4ERR_STALE_CLIENTID);

    let status = compound_status(
        &export,
        &[op_only(OP_PUTROOTFH), op_open_with_deny("README.txt", 1, 1)],
    );
    assert_eq!(status, NFS4ERR_NOTSUPP);
}

#[test]
fn synthetic_open_and_read_symlink_return_symlink() {
    let export = StaticExport::fixture();
    let open_status = compound_status(&export, &[op_only(OP_PUTROOTFH), op_open("readme-link", 1)]);
    assert_eq!(open_status, NFS4ERR_SYMLINK);

    let read_status = compound_status(
        &export,
        &[
            op_only(OP_PUTROOTFH),
            op_lookup("readme-link"),
            op_read([0; 16], 0, 64),
        ],
    );
    assert_eq!(read_status, NFS4ERR_SYMLINK);
}

#[test]
fn secinfo_uses_lookup_rules_without_changing_current_filehandle() {
    let export = StaticExport::fixture();
    assert_eq!(
        compound_status(&export, &[op_secinfo("README.txt")]),
        NFS4ERR_NOFILEHANDLE
    );
    assert_eq!(
        compound_status(&export, &[op_only(OP_PUTROOTFH), op_secinfo("")]),
        NFS4ERR_INVAL
    );
    assert_eq!(
        compound_status(&export, &[op_only(OP_PUTROOTFH), op_secinfo("missing")]),
        NFS4ERR_NOENT
    );
    assert_eq!(
        compound_status(
            &export,
            &[
                op_only(OP_PUTROOTFH),
                op_secinfo("README.txt"),
                op_lookup("README.txt")
            ],
        ),
        NFS4_OK
    );
    assert_eq!(
        compound_status(
            &export,
            &[
                op_only(OP_PUTROOTFH),
                op_lookup("README.txt"),
                op_secinfo("child")
            ],
        ),
        NFS4ERR_NOTDIR
    );
}

#[test]
fn close_rejects_unknown_stateid() {
    let export = StaticExport::fixture();
    let status = compound_status(&export, &[op_close([0; 16])]);
    assert_eq!(status, crate::protocol::consts::NFS4ERR_BAD_STATEID);
}

#[test]
fn open_confirm_rejects_arbitrary_stateid() {
    let export = StaticExport::fixture();
    let mut op = XdrWriter::new();
    op.u32(OP_OPEN_CONFIRM);
    op.bytes(&[0; 16]);
    op.u32(1);
    let status = compound_status(&export, &[op.into_inner()]);
    assert_eq!(status, crate::protocol::consts::NFS4ERR_BAD_STATEID);
}

#[test]
fn open_confirm_rejects_valid_stateid_as_unsupported() {
    let export = StaticExport::fixture();
    let open = compound_result(&export, &[op_only(OP_PUTROOTFH), op_open("README.txt", 1)]);
    let stateid = open_stateid(&open);
    let mut op = XdrWriter::new();
    op.u32(OP_OPEN_CONFIRM);
    op.bytes(&stateid);
    op.u32(1);
    let status = compound_status(&export, &[op.into_inner()]);
    assert_eq!(status, NFS4ERR_NOTSUPP);
}

#[test]
fn stale_generation_filehandle_expires() {
    let export = StaticExport::fixture();
    let mut op = XdrWriter::new();
    op.u32(OP_PUTFH);
    op.opaque(&file_handle(1, 2));
    let status = compound_status(&export, &[op.into_inner()]);
    assert_eq!(status, NFS4ERR_FHEXPIRED);
}

#[test]
fn unknown_opcode_returns_illegal_op_result() {
    let export = StaticExport::fixture();
    let result = compound_result(&export, &[op_only(999_999)]);
    let mut reader = XdrReader::new(&result);
    assert_eq!(reader.u32().unwrap(), NFS4ERR_OP_ILLEGAL);
    assert_eq!(reader.string().unwrap(), "test");
    assert_eq!(reader.u32().unwrap(), 1);
    assert_eq!(reader.u32().unwrap(), OP_ILLEGAL);
    assert_eq!(reader.u32().unwrap(), NFS4ERR_OP_ILLEGAL);
}

#[test]
fn minor_version_mismatch_returns_no_operation_results() {
    let export = StaticExport::fixture();
    let result = compound_result_with_minor(&export, 1, &[op_only(OP_PUTROOTFH)]);
    let mut reader = XdrReader::new(&result);
    assert_eq!(reader.u32().unwrap(), NFS4ERR_MINOR_VERS_MISMATCH);
    assert_eq!(reader.string().unwrap(), "test");
    assert_eq!(reader.u32().unwrap(), 0);
}

#[test]
fn getfh_putfh_roundtrip_recovers_original_object() {
    let export = StaticExport::fixture();
    let result = compound_result(
        &export,
        &[
            op_only(OP_PUTROOTFH),
            op_lookup("README.txt"),
            op_only(OP_GETFH),
        ],
    );
    let mut reader = XdrReader::new(&result);
    assert_eq!(reader.u32().unwrap(), NFS4_OK);
    assert_eq!(reader.string().unwrap(), "test");
    assert_eq!(reader.u32().unwrap(), 3);
    assert_eq!(reader.u32().unwrap(), OP_PUTROOTFH);
    assert_eq!(reader.u32().unwrap(), NFS4_OK);
    assert_eq!(reader.u32().unwrap(), OP_LOOKUP);
    assert_eq!(reader.u32().unwrap(), NFS4_OK);
    assert_eq!(reader.u32().unwrap(), OP_GETFH);
    assert_eq!(reader.u32().unwrap(), NFS4_OK);
    let handle = reader.opaque().unwrap();
    assert_eq!(
        decode_file_handle(TEST_GENERATION, &handle).expect("decode filehandle"),
        2
    );

    let mut putfh = XdrWriter::new();
    putfh.u32(OP_PUTFH);
    putfh.opaque(&handle);
    let status = compound_status(&export, &[putfh.into_inner(), op_getattr(&[FATTR4_FILEID])]);
    assert_eq!(status, NFS4_OK);
}

#[test]
fn getattr_encodes_requested_attrs() {
    let export = StaticExport::fixture();
    let result = compound_result(
        &export,
        &[
            op_only(OP_PUTROOTFH),
            op_lookup("README.txt"),
            op_getattr(&[FATTR4_TYPE, FATTR4_SIZE, FATTR4_FILEID]),
        ],
    );
    let mut reader = XdrReader::new(&result);
    assert_eq!(reader.u32().unwrap(), NFS4_OK);
    assert_eq!(reader.string().unwrap(), "test");
    assert_eq!(reader.u32().unwrap(), 3);
    assert_eq!(reader.u32().unwrap(), OP_PUTROOTFH);
    assert_eq!(reader.u32().unwrap(), NFS4_OK);
    assert_eq!(reader.u32().unwrap(), OP_LOOKUP);
    assert_eq!(reader.u32().unwrap(), NFS4_OK);
    assert_eq!(reader.u32().unwrap(), OP_GETATTR);
    assert_eq!(reader.u32().unwrap(), NFS4_OK);
    let (bits, vals) = reader.fattr().unwrap();
    assert_eq!(
        bits,
        request_words(&[FATTR4_TYPE, FATTR4_SIZE, FATTR4_FILEID])
    );
    let mut vals = XdrReader::new(&vals);
    assert_eq!(vals.u32().unwrap(), NF4REG);
    assert_eq!(vals.u64().unwrap(), 12);
    assert_eq!(vals.u64().unwrap(), 2);
}

#[test]
fn synthetic_read_uses_open_snapshot() {
    let mut export = StaticExport::fixture();
    let open = compound_result(&export, &[op_only(OP_PUTROOTFH), op_open("README.txt", 1)]);
    let stateid = StateId::from_wire(&open_stateid(&open)).expect("stateid");
    let StaticData::File(data) = &mut export.nodes.get_mut(&2).expect("file node").data else {
        panic!("README node should be a file");
    };
    *data = b"changed after open".to_vec();

    let read = export.read_state(stateid, 0, 64).expect("read open state");
    assert_eq!(read.data, b"hello nfs v4".to_vec());
    assert!(read.eof);
}
