// RPC reply status (RFC 5531 section 9)
pub(crate) const RPC_CALL: u32 = 0;
pub(crate) const RPC_REPLY: u32 = 1;
pub(crate) const RPC_MSG_ACCEPTED: u32 = 0;
pub(crate) const RPC_MSG_DENIED: u32 = 1;

// accept_stat values
pub(crate) const RPC_SUCCESS: u32 = 0;
pub(crate) const RPC_PROG_UNAVAIL: u32 = 1;
pub(crate) const RPC_PROG_MISMATCH: u32 = 2;
pub(crate) const RPC_PROC_UNAVAIL: u32 = 3;
pub(crate) const RPC_GARBAGE_ARGS: u32 = 4;

// reject_stat values for MSG_DENIED
pub(crate) const RPC_MISMATCH: u32 = 0;
pub(crate) const AUTH_ERROR: u32 = 1;

// auth_stat values for AUTH_ERROR denied body
pub(crate) const AUTH_BADCRED: u32 = 1;

// RPC auth flavors
pub(crate) const AUTH_NONE: u32 = 0;
pub(crate) const AUTH_SYS: u32 = 1;

// NFSv4 program identity
pub(crate) const NFS_PROGRAM: u32 = 100_003;
pub(crate) const NFS_VERSION_4: u32 = 4;
pub(crate) const NFS_VERSION_MIN: u32 = 4;
pub(crate) const NFS_VERSION_MAX: u32 = 4;
pub(crate) const PROC_NULL: u32 = 0;
pub(crate) const PROC_COMPOUND: u32 = 1;

// macOS Spotlight recognizes this root marker as a request not to index the
// mounted filesystem. It is synthetic and read-only: it never enters the
// provider namespace or the upstream cache.
pub(crate) const SPOTLIGHT_MARKER_NAME: &str = ".metadata_never_index";
pub(crate) const SPOTLIGHT_MARKER_ID: u64 = u64::MAX - 1;

pub(crate) fn is_reserved_inode(id: u64) -> bool {
    id == SPOTLIGHT_MARKER_ID
}

// Fragment reassembly: cumulative record cap. Single ONC RPC fragments are
// capped at 2 GiB by the header bit-width, but multi-fragment records have
// no inherent bound. 16 MiB covers NFSv4 READDIR replies and small file
// payloads; larger payloads should use dedicated protocols.
pub(crate) const MAX_RPC_RECORD_BYTES: u64 = 16 * 1024 * 1024;

pub const NFS4_OK: u32 = 0;
pub const NFS4ERR_NOENT: u32 = 2;
pub const NFS4ERR_IO: u32 = 5;
pub const NFS4ERR_ACCESS: u32 = 13;
pub const NFS4ERR_NOTDIR: u32 = 20;
pub const NFS4ERR_ISDIR: u32 = 21;
pub const NFS4ERR_INVAL: u32 = 22;
pub const NFS4ERR_ROFS: u32 = 30;
pub const NFS4ERR_STALE: u32 = 70;
pub(crate) const NFS4ERR_BADHANDLE: u32 = 10001;
pub(crate) const NFS4ERR_BAD_COOKIE: u32 = 10003;
pub(crate) const NFS4ERR_NOTSUPP: u32 = 10004;
pub(crate) const NFS4ERR_TOOSMALL: u32 = 10005;
pub(crate) const NFS4ERR_DELAY: u32 = 10008;
pub(crate) const NFS4ERR_EXPIRED: u32 = 10011;
pub(crate) const NFS4ERR_FHEXPIRED: u32 = 10014;
pub const NFS4ERR_RESOURCE: u32 = 10018;
pub(crate) const NFS4ERR_NOFILEHANDLE: u32 = 10020;
pub(crate) const NFS4ERR_MINOR_VERS_MISMATCH: u32 = 10021;
pub(crate) const NFS4ERR_STALE_CLIENTID: u32 = 10022;
pub(crate) const NFS4ERR_OLD_STATEID: u32 = 10024;
pub(crate) const NFS4ERR_BAD_STATEID: u32 = 10025;
pub(crate) const NFS4ERR_SYMLINK: u32 = 10029;
pub(crate) const NFS4ERR_NO_GRACE: u32 = 10033;
pub(crate) const NFS4ERR_OPENMODE: u32 = 10038;
pub(crate) const NFS4ERR_LOCK_NOTSUPP: u32 = 10043;
pub(crate) const NFS4ERR_OP_ILLEGAL: u32 = 10044;

pub(crate) const OP_ACCESS: u32 = 3;
pub(crate) const OP_CLOSE: u32 = 4;
pub(crate) const OP_COMMIT: u32 = 5;
pub(crate) const OP_CREATE: u32 = 6;
pub(crate) const OP_GETATTR: u32 = 9;
pub(crate) const OP_GETFH: u32 = 10;
pub(crate) const OP_LINK: u32 = 11;
pub(crate) const OP_LOCK: u32 = 12;
pub(crate) const OP_LOCKT: u32 = 13;
pub(crate) const OP_LOCKU: u32 = 14;
pub(crate) const OP_LOOKUP: u32 = 15;
pub(crate) const OP_LOOKUPP: u32 = 16;
pub(crate) const OP_OPEN: u32 = 18;
pub(crate) const OP_OPENATTR: u32 = 19;
pub(crate) const OP_OPEN_CONFIRM: u32 = 20;
pub(crate) const OP_OPEN_DOWNGRADE: u32 = 21;
pub(crate) const OP_PUTFH: u32 = 22;
pub(crate) const OP_PUTPUBFH: u32 = 23;
pub(crate) const OP_PUTROOTFH: u32 = 24;
pub(crate) const OP_READ: u32 = 25;
pub(crate) const OP_READDIR: u32 = 26;
pub(crate) const OP_READLINK: u32 = 27;
pub(crate) const OP_REMOVE: u32 = 28;
pub(crate) const OP_RENAME: u32 = 29;
pub(crate) const OP_RENEW: u32 = 30;
pub(crate) const OP_RESTOREFH: u32 = 31;
pub(crate) const OP_SAVEFH: u32 = 32;
pub(crate) const OP_SECINFO: u32 = 33;
pub(crate) const OP_SETATTR: u32 = 34;
pub(crate) const OP_SETCLIENTID: u32 = 35;
pub(crate) const OP_SETCLIENTID_CONFIRM: u32 = 36;
pub(crate) const OP_VERIFY: u32 = 37;
pub(crate) const OP_WRITE: u32 = 38;
pub(crate) const OP_RELEASE_LOCKOWNER: u32 = 39;
pub(crate) const OP_ILLEGAL: u32 = 10044;

pub(crate) const NF4REG: u32 = 1;
pub(crate) const NF4DIR: u32 = 2;
pub(crate) const NF4LNK: u32 = 5;

pub(crate) const FATTR4_SUPPORTED_ATTRS: u32 = 0;
pub(crate) const FATTR4_TYPE: u32 = 1;
pub(crate) const FATTR4_FH_EXPIRE_TYPE: u32 = 2;
pub(crate) const FATTR4_CHANGE: u32 = 3;
pub(crate) const FATTR4_SIZE: u32 = 4;
pub(crate) const FATTR4_LINK_SUPPORT: u32 = 5;
pub(crate) const FATTR4_SYMLINK_SUPPORT: u32 = 6;
pub(crate) const FATTR4_NAMED_ATTR: u32 = 7;
pub(crate) const FATTR4_FSID: u32 = 8;
pub(crate) const FATTR4_UNIQUE_HANDLES: u32 = 9;
pub(crate) const FATTR4_LEASE_TIME: u32 = 10;
#[allow(dead_code)]
pub(crate) const FATTR4_RDATTR_ERROR: u32 = 11;
pub(crate) const FATTR4_ACLSUPPORT: u32 = 13;
pub(crate) const FATTR4_CANSETTIME: u32 = 15;
pub(crate) const FATTR4_CASE_INSENSITIVE: u32 = 16;
pub(crate) const FATTR4_CASE_PRESERVING: u32 = 17;
pub(crate) const FATTR4_CHOWN_RESTRICTED: u32 = 18;
pub(crate) const FATTR4_FILEHANDLE: u32 = 19;
pub(crate) const FATTR4_FILEID: u32 = 20;
pub(crate) const FATTR4_FILES_AVAIL: u32 = 21;
pub(crate) const FATTR4_FILES_FREE: u32 = 22;
pub(crate) const FATTR4_FILES_TOTAL: u32 = 23;
pub(crate) const FATTR4_HOMOGENEOUS: u32 = 26;
pub(crate) const FATTR4_MAXFILESIZE: u32 = 27;
pub(crate) const FATTR4_MAXLINK: u32 = 28;
pub(crate) const FATTR4_MAXNAME: u32 = 29;
pub(crate) const FATTR4_MAXREAD: u32 = 30;
pub(crate) const FATTR4_MAXWRITE: u32 = 31;
pub(crate) const FATTR4_MODE: u32 = 33;
pub(crate) const FATTR4_NO_TRUNC: u32 = 34;
pub(crate) const FATTR4_NUMLINKS: u32 = 35;
pub(crate) const FATTR4_OWNER: u32 = 36;
pub(crate) const FATTR4_OWNER_GROUP: u32 = 37;
pub(crate) const FATTR4_RAWDEV: u32 = 41;
pub(crate) const FATTR4_SPACE_AVAIL: u32 = 42;
pub(crate) const FATTR4_SPACE_FREE: u32 = 43;
pub(crate) const FATTR4_SPACE_TOTAL: u32 = 44;
pub(crate) const FATTR4_SPACE_USED: u32 = 45;
pub(crate) const FATTR4_TIME_ACCESS: u32 = 47;
pub(crate) const FATTR4_TIME_DELTA: u32 = 51;
pub(crate) const FATTR4_TIME_METADATA: u32 = 52;
pub(crate) const FATTR4_TIME_MODIFY: u32 = 53;
#[allow(dead_code)]
pub(crate) const FATTR4_MOUNTED_ON_FILEID: u32 = 55;

pub(crate) const FH4_VOLATILE_ANY: u32 = 0x0000_0002;

pub(crate) const ACCESS4_READ: u32 = 0x0001;
pub(crate) const ACCESS4_LOOKUP: u32 = 0x0002;
pub(crate) const ACCESS4_MODIFY: u32 = 0x0004;
pub(crate) const ACCESS4_EXTEND: u32 = 0x0008;
pub(crate) const ACCESS4_DELETE: u32 = 0x0010;
pub(crate) const ACCESS4_EXECUTE: u32 = 0x0020;

pub(crate) const OPEN4_SHARE_ACCESS_READ: u32 = 0x0000_0001;
pub(crate) const OPEN4_SHARE_ACCESS_WRITE: u32 = 0x0000_0002;
pub(crate) const OPEN4_SHARE_DENY_NONE: u32 = 0x0000_0000;
pub(crate) const OPEN_DELEGATE_NONE: u32 = 0;
pub(crate) const UNCHECKED4: u32 = 0;
pub(crate) const GUARDED4: u32 = 1;
pub(crate) const EXCLUSIVE4: u32 = 2;
pub(crate) const CLAIM_NULL: u32 = 0;
pub(crate) const CLAIM_PREVIOUS: u32 = 1;
pub(crate) const CLAIM_DELEGATE_CUR: u32 = 2;
pub(crate) const CLAIM_DELEGATE_PREV: u32 = 3;
pub(crate) const CLAIM_FH: u32 = 4;

pub(crate) const NFS_EXPORT_NAME: &str = "omnifs";
pub(crate) const ROOT_ID: u64 = 1;
pub(crate) const EXPORT_ROOT_ID: u64 = 2;
pub(crate) const OPEN_STATE_LEASE_SECONDS: u64 = 10;
pub(crate) const MAX_NFS_READ_BYTES: u32 = 1024 * 1024;
