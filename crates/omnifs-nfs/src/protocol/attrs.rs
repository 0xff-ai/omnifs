use crate::export::{Attr, NodeKind, Status};
use crate::protocol::consts::{
    FATTR4_ACLSUPPORT, FATTR4_CANSETTIME, FATTR4_CASE_INSENSITIVE, FATTR4_CASE_PRESERVING,
    FATTR4_CHANGE, FATTR4_CHOWN_RESTRICTED, FATTR4_FH_EXPIRE_TYPE, FATTR4_FILEHANDLE,
    FATTR4_FILEID, FATTR4_FILES_AVAIL, FATTR4_FILES_FREE, FATTR4_FILES_TOTAL, FATTR4_FSID,
    FATTR4_HOMOGENEOUS, FATTR4_LEASE_TIME, FATTR4_LINK_SUPPORT, FATTR4_MAXFILESIZE, FATTR4_MAXLINK,
    FATTR4_MAXNAME, FATTR4_MAXREAD, FATTR4_MAXWRITE, FATTR4_MODE, FATTR4_NAMED_ATTR,
    FATTR4_NO_TRUNC, FATTR4_NUMLINKS, FATTR4_OWNER, FATTR4_OWNER_GROUP, FATTR4_RAWDEV, FATTR4_SIZE,
    FATTR4_SPACE_AVAIL, FATTR4_SPACE_FREE, FATTR4_SPACE_TOTAL, FATTR4_SPACE_USED,
    FATTR4_SUPPORTED_ATTRS, FATTR4_SYMLINK_SUPPORT, FATTR4_TIME_ACCESS, FATTR4_TIME_DELTA,
    FATTR4_TIME_METADATA, FATTR4_TIME_MODIFY, FATTR4_TYPE, FATTR4_UNIQUE_HANDLES, FH4_VOLATILE_ANY,
    MAX_NFS_READ_BYTES, NFS4_OK, OP_ACCESS, OP_CLOSE, OP_COMMIT, OP_CREATE, OP_GETATTR, OP_GETFH,
    OP_ILLEGAL, OP_LINK, OP_LOCK, OP_LOCKT, OP_LOCKU, OP_LOOKUP, OP_LOOKUPP, OP_OPEN,
    OP_OPEN_CONFIRM, OP_OPEN_DOWNGRADE, OP_OPENATTR, OP_PUTFH, OP_PUTPUBFH, OP_PUTROOTFH, OP_READ,
    OP_READDIR, OP_READLINK, OP_RELEASE_LOCKOWNER, OP_REMOVE, OP_RENAME, OP_RENEW, OP_RESTOREFH,
    OP_SAVEFH, OP_SECINFO, OP_SETATTR, OP_SETCLIENTID, OP_SETCLIENTID_CONFIRM, OP_VERIFY, OP_WRITE,
};
use crate::protocol::filehandle::file_handle;
use crate::protocol::xdr::XdrWriter;

const SUPPORTED_ATTR_BITS: &[u32] = &[
    FATTR4_SUPPORTED_ATTRS,
    FATTR4_TYPE,
    FATTR4_FH_EXPIRE_TYPE,
    FATTR4_CHANGE,
    FATTR4_SIZE,
    FATTR4_LINK_SUPPORT,
    FATTR4_SYMLINK_SUPPORT,
    FATTR4_NAMED_ATTR,
    FATTR4_FSID,
    FATTR4_UNIQUE_HANDLES,
    FATTR4_LEASE_TIME,
    FATTR4_ACLSUPPORT,
    FATTR4_CANSETTIME,
    FATTR4_CASE_INSENSITIVE,
    FATTR4_CASE_PRESERVING,
    FATTR4_CHOWN_RESTRICTED,
    FATTR4_FILEHANDLE,
    FATTR4_FILEID,
    FATTR4_FILES_AVAIL,
    FATTR4_FILES_FREE,
    FATTR4_FILES_TOTAL,
    FATTR4_HOMOGENEOUS,
    FATTR4_MAXFILESIZE,
    FATTR4_MAXLINK,
    FATTR4_MAXNAME,
    FATTR4_MAXREAD,
    FATTR4_MAXWRITE,
    FATTR4_MODE,
    FATTR4_NO_TRUNC,
    FATTR4_NUMLINKS,
    FATTR4_OWNER,
    FATTR4_OWNER_GROUP,
    FATTR4_RAWDEV,
    FATTR4_SPACE_AVAIL,
    FATTR4_SPACE_FREE,
    FATTR4_SPACE_TOTAL,
    FATTR4_SPACE_USED,
    FATTR4_TIME_ACCESS,
    FATTR4_TIME_DELTA,
    FATTR4_TIME_METADATA,
    FATTR4_TIME_MODIFY,
];

pub(crate) fn encode_access(status: Result<(u32, u32), Status>) -> (u32, Vec<u8>) {
    match status {
        Ok((supported, access)) => {
            let mut res = op_status(OP_ACCESS, NFS4_OK);
            res.u32(supported);
            res.u32(access);
            (NFS4_OK, res.into_inner())
        },
        Err(status) => (
            status.wire(),
            op_status(OP_ACCESS, status.wire()).into_inner(),
        ),
    }
}

pub(crate) fn encode_getattr(status: Result<Vec<u8>, Status>) -> (u32, Vec<u8>) {
    match status {
        Ok(fattr) => {
            let mut res = op_status(OP_GETATTR, NFS4_OK);
            res.bytes(&fattr);
            (NFS4_OK, res.into_inner())
        },
        Err(status) => (
            status.wire(),
            op_status(OP_GETATTR, status.wire()).into_inner(),
        ),
    }
}

pub(crate) fn encode_getfh(status: Result<Vec<u8>, Status>) -> (u32, Vec<u8>) {
    match status {
        Ok(fh) => {
            let mut res = op_status(OP_GETFH, NFS4_OK);
            res.opaque(&fh);
            (NFS4_OK, res.into_inner())
        },
        Err(status) => (
            status.wire(),
            op_status(OP_GETFH, status.wire()).into_inner(),
        ),
    }
}

pub(crate) fn op_status(op: u32, status: u32) -> XdrWriter {
    let mut res = XdrWriter::new();
    res.u32(op);
    res.u32(status);
    res
}

pub(crate) fn status_to_u32(status: Result<(), Status>) -> u32 {
    status.err().map_or(NFS4_OK, Status::wire)
}

#[allow(clippy::match_same_arms)]
pub(crate) fn encode_attrs(generation: u64, attr: &Attr, request: &[u32]) -> Vec<u8> {
    let mut result_bits = Vec::new();
    let mut vals = XdrWriter::new();
    for bit in requested_bits(request) {
        if !SUPPORTED_ATTR_BITS.contains(&bit) {
            continue;
        }
        result_bits.push(bit);
        match bit {
            FATTR4_SUPPORTED_ATTRS => encode_bitmap(&mut vals, SUPPORTED_ATTR_BITS),
            FATTR4_TYPE => vals.u32(attr.kind.nfs_type()),
            FATTR4_FH_EXPIRE_TYPE => vals.u32(FH4_VOLATILE_ANY),
            FATTR4_CHANGE => vals.u64(attr.change),
            FATTR4_SIZE => vals.u64(attr.size),
            FATTR4_LINK_SUPPORT => vals.bool(true),
            FATTR4_SYMLINK_SUPPORT => vals.bool(true),
            FATTR4_NAMED_ATTR => vals.bool(false),
            FATTR4_FSID => {
                vals.u64(0x4f4d_4e49_4653);
                vals.u64(1);
            },
            FATTR4_UNIQUE_HANDLES => vals.bool(true),
            FATTR4_LEASE_TIME => vals.u32(10),
            FATTR4_ACLSUPPORT => vals.u32(0),
            FATTR4_CANSETTIME => vals.bool(false),
            FATTR4_CASE_INSENSITIVE => vals.bool(false),
            FATTR4_CASE_PRESERVING => vals.bool(true),
            FATTR4_CHOWN_RESTRICTED => vals.bool(true),
            FATTR4_FILEHANDLE => vals.opaque(&file_handle(generation, attr.id)),
            FATTR4_FILEID => vals.u64(attr.id),
            FATTR4_FILES_AVAIL => vals.u64(1_000_000),
            FATTR4_FILES_FREE => vals.u64(1_000_000),
            FATTR4_FILES_TOTAL => vals.u64(1_000_000),
            FATTR4_HOMOGENEOUS => vals.bool(true),
            FATTR4_MAXFILESIZE => vals.u64(1 << 40),
            FATTR4_MAXLINK => vals.u32(1),
            FATTR4_MAXNAME => vals.u32(255),
            FATTR4_MAXREAD => vals.u64(u64::from(MAX_NFS_READ_BYTES)),
            FATTR4_MAXWRITE => vals.u64(0),
            FATTR4_MODE => vals.u32(attr.mode),
            FATTR4_NO_TRUNC => vals.bool(true),
            // Compatibility simplification: computing child directory counts
            // requires listing the directory during attribute encoding.
            FATTR4_NUMLINKS => vals.u32(if attr.kind == NodeKind::Directory {
                2
            } else {
                1
            }),
            FATTR4_OWNER => vals.string("0"),
            FATTR4_OWNER_GROUP => vals.string("0"),
            FATTR4_RAWDEV => {
                vals.u32(0);
                vals.u32(0);
            },
            FATTR4_SPACE_AVAIL => vals.u64(1 << 30),
            FATTR4_SPACE_FREE => vals.u64(1 << 30),
            FATTR4_SPACE_TOTAL => vals.u64(1 << 30),
            FATTR4_SPACE_USED => vals.u64(attr.size),
            FATTR4_TIME_ACCESS | FATTR4_TIME_METADATA | FATTR4_TIME_MODIFY => {
                vals.i64(attr.mtime_sec);
                vals.u32(0);
            },
            FATTR4_TIME_DELTA => {
                vals.i64(1);
                vals.u32(0);
            },
            _ => {},
        }
    }

    let attr_vals = vals.into_inner();
    let mut out = XdrWriter::new();
    encode_bitmap(&mut out, &result_bits);
    out.opaque(&attr_vals);
    out.into_inner()
}

fn requested_bits(words: &[u32]) -> impl Iterator<Item = u32> + '_ {
    words
        .iter()
        .copied()
        .enumerate()
        .flat_map(|(word_idx, word)| {
            (0..32).filter_map(move |bit| {
                if word & (1 << bit) == 0 {
                    return None;
                }
                Some(
                    u32::try_from(word_idx)
                        .expect("attribute bitmap index exceeds u32")
                        .saturating_mul(32)
                        + bit,
                )
            })
        })
}

pub(crate) fn encode_bitmap(writer: &mut XdrWriter, bits: &[u32]) {
    let max_word = bits.iter().map(|bit| bit / 32).max();
    let Some(max_word) = max_word else {
        writer.u32(0);
        return;
    };

    writer.u32(max_word + 1);
    for word_idx in 0..=max_word {
        let word = bits
            .iter()
            .copied()
            .filter(|bit| bit / 32 == word_idx)
            .fold(0_u32, |word, bit| word | (1 << (bit % 32)));
        writer.u32(word);
    }
}

pub(crate) fn op_name(op: u32) -> &'static str {
    match op {
        OP_ACCESS => "ACCESS",
        OP_CLOSE => "CLOSE",
        OP_COMMIT => "COMMIT",
        OP_CREATE => "CREATE",
        OP_GETATTR => "GETATTR",
        OP_GETFH => "GETFH",
        OP_LINK => "LINK",
        OP_LOCK => "LOCK",
        OP_LOCKT => "LOCKT",
        OP_LOCKU => "LOCKU",
        OP_LOOKUP => "LOOKUP",
        OP_LOOKUPP => "LOOKUPP",
        OP_OPEN => "OPEN",
        OP_OPENATTR => "OPENATTR",
        OP_OPEN_CONFIRM => "OPEN_CONFIRM",
        OP_OPEN_DOWNGRADE => "OPEN_DOWNGRADE",
        OP_PUTFH => "PUTFH",
        OP_PUTPUBFH => "PUTPUBFH",
        OP_PUTROOTFH => "PUTROOTFH",
        OP_READ => "READ",
        OP_READDIR => "READDIR",
        OP_READLINK => "READLINK",
        OP_REMOVE => "REMOVE",
        OP_RENAME => "RENAME",
        OP_RENEW => "RENEW",
        OP_RESTOREFH => "RESTOREFH",
        OP_SAVEFH => "SAVEFH",
        OP_SECINFO => "SECINFO",
        OP_SETATTR => "SETATTR",
        OP_SETCLIENTID => "SETCLIENTID",
        OP_SETCLIENTID_CONFIRM => "SETCLIENTID_CONFIRM",
        OP_VERIFY => "VERIFY",
        OP_WRITE => "WRITE",
        OP_RELEASE_LOCKOWNER => "RELEASE_LOCKOWNER",
        OP_ILLEGAL => "ILLEGAL",
        _ => "UNKNOWN",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::consts::{
        FATTR4_MOUNTED_ON_FILEID, FATTR4_RDATTR_ERROR, FH4_VOLATILE_ANY,
    };
    use crate::protocol::xdr::XdrReader;

    fn attr() -> Attr {
        Attr {
            id: 42,
            parent: 1,
            kind: NodeKind::File,
            size: 5,
            mode: 0o444,
            change: 9,
            mtime_sec: 0,
        }
    }

    fn request_words(bits: &[u32]) -> Vec<u32> {
        let mut writer = XdrWriter::new();
        encode_bitmap(&mut writer, bits);
        let bytes = writer.into_inner();
        XdrReader::new(&bytes).bitmap().unwrap()
    }

    fn encode_requested(bits: &[u32]) -> (Vec<u32>, Vec<u8>) {
        let request = request_words(bits);
        XdrReader::new(&encode_attrs(7, &attr(), &request))
            .fattr()
            .unwrap()
    }

    #[test]
    fn filehandles_advertise_volatile_expiration() {
        let (bits, vals) = encode_requested(&[FATTR4_FH_EXPIRE_TYPE]);
        assert_eq!(bits, request_words(&[FATTR4_FH_EXPIRE_TYPE]));
        let mut vals = XdrReader::new(&vals);
        assert_eq!(vals.u32().unwrap(), FH4_VOLATILE_ANY);
    }

    #[test]
    fn time_delta_matches_whole_second_mtime_granularity() {
        let (_bits, vals) = encode_requested(&[FATTR4_TIME_DELTA]);
        let mut vals = XdrReader::new(&vals);
        let seconds = i64::from_be_bytes(vals.take(8).unwrap().try_into().expect("fixed i64"));
        assert_eq!(seconds, 1);
        assert_eq!(vals.u32().unwrap(), 0);
    }

    #[test]
    fn unsupported_contextual_attrs_are_not_advertised() {
        let (bits, vals) = encode_requested(&[FATTR4_RDATTR_ERROR, FATTR4_MOUNTED_ON_FILEID]);
        assert!(bits.is_empty());
        assert!(vals.is_empty());
    }
}
