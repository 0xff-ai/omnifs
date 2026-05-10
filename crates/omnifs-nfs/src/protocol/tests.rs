use crate::export::{NfsAttr, NfsDirEntry, NfsNodeKind, NfsResult, ReadOnlyExport};
use crate::protocol::attrs::encode_bitmap;
use crate::protocol::compound::handle_compound;
use crate::protocol::consts::{
    CLAIM_NULL, CLIENT_ID, FATTR4_FILEID, FATTR4_SIZE, FATTR4_TYPE, NFS4_OK, NFS4ERR_INVAL,
    NFS4ERR_ISDIR, NFS4ERR_NOENT, NFS4ERR_NOTDIR, NFS4ERR_RESOURCE, NFS4ERR_ROFS, NFS4ERR_STALE,
    OP_CLOSE, OP_GETATTR, OP_LOOKUP, OP_OPEN, OP_PUTFH, OP_PUTROOTFH, OP_READ, OP_READDIR,
    OP_READLINK, READDIR_COOKIE_VERIFIER,
};
use crate::protocol::filehandle::{file_handle, now_sec};
use crate::protocol::ops::handle_readdir;
use crate::protocol::xdr::{XdrReader, XdrWriter};
use crate::trace::Trace;
use std::collections::BTreeMap;

#[derive(Clone)]
struct StaticNode {
    attr: NfsAttr,
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
    open_limit: Option<usize>,
}

impl StaticExport {
    fn fixture() -> Self {
        let now = now_sec();
        let mut nodes = BTreeMap::new();
        nodes.insert(
            1,
            StaticNode {
                name: String::new(),
                attr: NfsAttr {
                    id: 1,
                    parent: 1,
                    kind: NfsNodeKind::Directory,
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
                attr: NfsAttr {
                    id: 2,
                    parent: 1,
                    kind: NfsNodeKind::File,
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
                attr: NfsAttr {
                    id: 3,
                    parent: 1,
                    kind: NfsNodeKind::Symlink,
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
            open_limit: None,
        }
    }

    fn fixture_with_open_limit(open_limit: usize) -> Self {
        Self {
            open_limit: Some(open_limit),
            ..Self::fixture()
        }
    }
}

impl ReadOnlyExport for StaticExport {
    fn root(&self) -> u64 {
        1
    }

    fn attr(&self, id: u64) -> NfsResult<NfsAttr> {
        self.nodes
            .get(&id)
            .map(|node| node.attr.clone())
            .ok_or(NFS4ERR_STALE)
    }

    fn lookup(&self, parent: u64, name: &str) -> NfsResult<u64> {
        match name {
            "." => return Ok(parent),
            ".." => return self.parent(parent),
            _ => {},
        }
        let parent = self.nodes.get(&parent).ok_or(NFS4ERR_STALE)?;
        let StaticData::Dir(children) = &parent.data else {
            return Err(NFS4ERR_NOTDIR);
        };
        children
            .iter()
            .copied()
            .find(|id| self.nodes.get(id).is_some_and(|node| node.name == name))
            .ok_or(NFS4ERR_NOENT)
    }

    fn readdir(&self, id: u64) -> NfsResult<Vec<NfsDirEntry>> {
        let node = self.nodes.get(&id).ok_or(NFS4ERR_STALE)?;
        let StaticData::Dir(children) = &node.data else {
            return Err(NFS4ERR_NOTDIR);
        };
        Ok(children
            .iter()
            .filter_map(|id| self.nodes.get(id))
            .map(|node| NfsDirEntry {
                id: node.attr.id,
                name: node.name.clone(),
                attr: node.attr.clone(),
            })
            .collect())
    }

    fn read(&self, id: u64) -> NfsResult<Vec<u8>> {
        match &self.nodes.get(&id).ok_or(NFS4ERR_STALE)?.data {
            StaticData::File(data) => Ok(data.clone()),
            StaticData::Dir(_) => Err(NFS4ERR_ISDIR),
            StaticData::Symlink(_) => Err(NFS4ERR_INVAL),
        }
    }

    fn readlink(&self, id: u64) -> NfsResult<Vec<u8>> {
        match &self.nodes.get(&id).ok_or(NFS4ERR_STALE)?.data {
            StaticData::Symlink(data) => Ok(data.clone()),
            _ => Err(NFS4ERR_INVAL),
        }
    }

    fn materialize_for_open(&self, id: u64, limit: usize) -> NfsResult<usize> {
        let limit = self.open_limit.unwrap_or(limit);
        let data = self.read(id)?;
        if data.len() > limit {
            return Err(NFS4ERR_RESOURCE);
        }
        Ok(data.len())
    }
}

fn compound_payload(ops: &[Vec<u8>]) -> Vec<u8> {
    let mut writer = XdrWriter::new();
    writer.string("test");
    writer.u32(0);
    writer.u32(ops.len() as u32);
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

fn op_lookup(name: &str) -> Vec<u8> {
    let mut writer = XdrWriter::new();
    writer.u32(OP_LOOKUP);
    writer.string(name);
    writer.into_inner()
}

fn op_open(name: &str, share_access: u32) -> Vec<u8> {
    let mut writer = XdrWriter::new();
    writer.u32(OP_OPEN);
    writer.u32(1);
    writer.u32(share_access);
    writer.u32(0);
    writer.u64(CLIENT_ID);
    writer.opaque(b"owner");
    writer.u32(0);
    writer.u32(CLAIM_NULL);
    writer.string(name);
    writer.into_inner()
}

fn op_read(offset: u64, count: u32) -> Vec<u8> {
    let mut writer = XdrWriter::new();
    writer.u32(OP_READ);
    writer.bytes(&[0; 16]);
    writer.u64(offset);
    writer.u32(count);
    writer.into_inner()
}

fn op_close() -> Vec<u8> {
    let mut writer = XdrWriter::new();
    writer.u32(OP_CLOSE);
    writer.u32(1);
    writer.bytes(&[0; 16]);
    writer.into_inner()
}

fn op_readdir() -> Vec<u8> {
    op_readdir_with_maxcount(4096)
}

fn op_readdir_with_maxcount(maxcount: u32) -> Vec<u8> {
    let mut writer = XdrWriter::new();
    writer.u32(OP_READDIR);
    writer.u64(0);
    writer.bytes(&READDIR_COOKIE_VERIFIER);
    writer.u32(4096);
    writer.u32(maxcount);
    encode_bitmap(&mut writer, &[FATTR4_TYPE, FATTR4_SIZE, FATTR4_FILEID]);
    writer.into_inner()
}

fn compound_status(export: &dyn ReadOnlyExport, ops: &[Vec<u8>]) -> u32 {
    let payload = compound_payload(ops);
    let mut reader = XdrReader::new(&payload);
    let result = handle_compound(&mut reader, 7, export, 1, &Trace::new(None).unwrap()).unwrap();
    let mut reader = XdrReader::new(&result);
    reader.u32().unwrap()
}

#[test]
fn synthetic_lookup_read_close_compound_succeeds() {
    let export = StaticExport::fixture();
    let status = compound_status(
        &export,
        &[
            op_only(OP_PUTROOTFH),
            op_open("README.txt", 1),
            op_read(0, 64),
            op_close(),
        ],
    );
    assert_eq!(status, NFS4_OK);
}

#[test]
fn synthetic_readdir_and_readlink_succeed() {
    let export = StaticExport::fixture();
    let readdir = compound_status(&export, &[op_only(OP_PUTROOTFH), op_readdir()]);
    assert_eq!(readdir, NFS4_OK);
    let readlink = compound_status(
        &export,
        &[
            op_only(OP_PUTROOTFH),
            op_lookup("readme-link"),
            op_only(OP_READLINK),
        ],
    );
    assert_eq!(readlink, NFS4_OK);
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
        &READDIR_COOKIE_VERIFIER,
        96,
        &[FATTR4_TYPE, FATTR4_SIZE, FATTR4_FILEID],
    );
    assert_eq!(status, NFS4_OK);
    assert!(result.len() <= 96);

    let mut reader = XdrReader::new(&result);
    assert_eq!(reader.u32().unwrap(), OP_READDIR);
    assert_eq!(reader.u32().unwrap(), NFS4_OK);
    assert_eq!(
        reader.fixed_opaque(8).unwrap(),
        READDIR_COOKIE_VERIFIER.to_vec()
    );
    let mut entries = 0;
    while reader.u32().unwrap() != 0 {
        entries += 1;
        let _cookie = reader.u64().unwrap();
        let _name = reader.string().unwrap();
        let _attrs = reader.bitmap().unwrap();
        let _attr_vals = reader.opaque().unwrap();
    }
    assert_eq!(entries, 1);
    assert_eq!(reader.u32().unwrap(), 0);
}

#[test]
fn synthetic_readdir_on_file_returns_notdir() {
    let export = StaticExport::fixture();
    let status = compound_status(
        &export,
        &[op_only(OP_PUTROOTFH), op_lookup("README.txt"), op_readdir()],
    );
    assert_eq!(status, NFS4ERR_NOTDIR);
}

#[test]
fn synthetic_write_open_returns_read_only() {
    let export = StaticExport::fixture();
    let status = compound_status(&export, &[op_only(OP_PUTROOTFH), op_open("README.txt", 2)]);
    assert_eq!(status, NFS4ERR_ROFS);
}

#[test]
fn synthetic_open_and_read_symlink_return_invalid() {
    let export = StaticExport::fixture();
    let open_status = compound_status(&export, &[op_only(OP_PUTROOTFH), op_open("readme-link", 1)]);
    assert_eq!(open_status, NFS4ERR_INVAL);

    let read_status = compound_status(
        &export,
        &[
            op_only(OP_PUTROOTFH),
            op_lookup("readme-link"),
            op_read(0, 64),
        ],
    );
    assert_eq!(read_status, NFS4ERR_INVAL);
}

#[test]
fn synthetic_open_materialization_limit_returns_resource() {
    let export = StaticExport::fixture_with_open_limit(4);
    let status = compound_status(&export, &[op_only(OP_PUTROOTFH), op_open("README.txt", 1)]);
    assert_eq!(status, NFS4ERR_RESOURCE);
}

#[test]
fn stale_generation_filehandle_fails() {
    let export = StaticExport::fixture();
    let mut op = XdrWriter::new();
    op.u32(OP_PUTFH);
    op.opaque(&file_handle(1, 2));
    let status = compound_status(&export, &[op.into_inner()]);
    assert_eq!(status, NFS4ERR_STALE);
}

#[test]
fn getattr_encodes_requested_attrs() {
    let export = StaticExport::fixture();
    let status = compound_status(
        &export,
        &[
            op_only(OP_PUTROOTFH),
            op_lookup("README.txt"),
            op_getattr(&[FATTR4_TYPE, FATTR4_SIZE, FATTR4_FILEID]),
        ],
    );
    assert_eq!(status, NFS4_OK);
}
