use crate::export::{DirEntry, DirListing, NodeKind, ReadOnlyExport, StateId, Status};
use crate::protocol::attrs::{
    encode_access, encode_attrs, encode_bitmap, encode_getattr, encode_getfh, op_status,
    status_to_u32,
};
use crate::protocol::client::ClientTable;
use crate::protocol::compound::CompoundState;
use crate::protocol::consts::{
    ACCESS4_DELETE, ACCESS4_EXECUTE, ACCESS4_EXTEND, ACCESS4_LOOKUP, ACCESS4_MODIFY, ACCESS4_READ,
    AUTH_NONE, AUTH_SYS, CLAIM_DELEGATE_CUR, CLAIM_DELEGATE_PREV, CLAIM_FH, CLAIM_NULL,
    CLAIM_PREVIOUS, EXCLUSIVE4, GUARDED4, NFS4_OK, OP_ACCESS, OP_CLOSE, OP_COMMIT, OP_CREATE,
    OP_GETATTR, OP_GETFH, OP_ILLEGAL, OP_LINK, OP_LOCK, OP_LOCKT, OP_LOCKU, OP_LOOKUP, OP_LOOKUPP,
    OP_OPEN, OP_OPEN_CONFIRM, OP_PUTFH, OP_PUTPUBFH, OP_PUTROOTFH, OP_READ, OP_READDIR,
    OP_READLINK, OP_RELEASE_LOCKOWNER, OP_REMOVE, OP_RENAME, OP_RENEW, OP_RESTOREFH, OP_SAVEFH,
    OP_SECINFO, OP_SETATTR, OP_SETCLIENTID, OP_SETCLIENTID_CONFIRM, OP_VERIFY, OP_WRITE,
    OPEN_DELEGATE_NONE, OPEN4_SHARE_ACCESS_READ, OPEN4_SHARE_ACCESS_WRITE, OPEN4_SHARE_DENY_NONE,
    UNCHECKED4,
};
use crate::protocol::filehandle::{decode_file_handle, file_handle};
use crate::protocol::name::ComponentName;
use crate::protocol::xdr::{XdrError, XdrReader, XdrWriter};
use std::hash::{DefaultHasher, Hash, Hasher};

const OPEN4_NOCREATE: u32 = 0;
const OPEN4_CREATE: u32 = 1;

#[derive(Debug, Clone, Copy)]
struct AccessMask(u32);

impl AccessMask {
    const READ_ONLY_SUPPORTED: Self = Self(
        ACCESS4_READ
            | ACCESS4_LOOKUP
            | ACCESS4_MODIFY
            | ACCESS4_EXTEND
            | ACCESS4_DELETE
            | ACCESS4_EXECUTE,
    );

    fn allowed_for(self, kind: NodeKind) -> Self {
        Self(self.0 & kind.allowed_access())
    }

    fn raw(self) -> u32 {
        self.0
    }
}

#[derive(Debug, Clone, Copy)]
struct ShareAccess(u32);

impl ShareAccess {
    const KNOWN_BITS: u32 = OPEN4_SHARE_ACCESS_READ | OPEN4_SHARE_ACCESS_WRITE;

    fn is_valid(self) -> bool {
        self.0 != 0 && self.0 & !Self::KNOWN_BITS == 0
    }

    fn allows_read(self) -> bool {
        self.0 & OPEN4_SHARE_ACCESS_READ != 0
    }

    fn allows_write(self) -> bool {
        self.0 & OPEN4_SHARE_ACCESS_WRITE != 0
    }

    fn raw(self) -> u32 {
        self.0
    }
}

#[derive(Debug, Clone, Copy)]
struct ShareDeny(u32);

impl ShareDeny {
    fn is_none(self) -> bool {
        self.0 == OPEN4_SHARE_DENY_NONE
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenType {
    NoCreate,
    Create,
    Unsupported,
}

impl OpenType {
    fn read(reader: &mut XdrReader<'_>) -> Result<Self, XdrError> {
        match reader.u32()? {
            OPEN4_NOCREATE => Ok(Self::NoCreate),
            OPEN4_CREATE => {
                match reader.u32()? {
                    UNCHECKED4 | GUARDED4 => {
                        let _ = reader.fattr()?;
                    },
                    EXCLUSIVE4 => {
                        let _ = reader.fixed_opaque(8)?;
                    },
                    _ => return Ok(Self::Unsupported),
                }
                Ok(Self::Create)
            },
            _ => Ok(Self::Unsupported),
        }
    }

    fn is_create(self) -> bool {
        matches!(self, Self::Create)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaimType {
    Null,
    Previous,
    DelegateCur,
    DelegatePrev,
    Fh,
    Unsupported,
}

impl ClaimType {
    fn from_wire(value: u32) -> Self {
        match value {
            CLAIM_NULL => Self::Null,
            CLAIM_PREVIOUS => Self::Previous,
            CLAIM_DELEGATE_CUR => Self::DelegateCur,
            CLAIM_DELEGATE_PREV => Self::DelegatePrev,
            CLAIM_FH => Self::Fh,
            _ => Self::Unsupported,
        }
    }
}

pub(crate) fn handle_op(
    op: u32,
    reader: &mut XdrReader<'_>,
    generation: u64,
    clients: &ClientTable,
    export: &dyn ReadOnlyExport,
    state: &mut CompoundState,
) -> Result<(u32, Vec<u8>), XdrError> {
    match op {
        OP_ACCESS => handle_access(reader, export, state),
        OP_CLOSE => handle_close(reader, export),
        OP_COMMIT | OP_CREATE | OP_LINK | OP_REMOVE | OP_RENAME | OP_SETATTR | OP_WRITE => {
            Ok(error_reply(op, Status::ReadOnlyFs))
        },
        OP_GETATTR => handle_getattr(reader, generation, export, state),
        OP_GETFH => Ok(handle_getfh(generation, export, state)),
        OP_LOCK | OP_LOCKT | OP_LOCKU | OP_RELEASE_LOCKOWNER => {
            Ok(error_reply(op, Status::LockNotSupported))
        },
        OP_LOOKUP => handle_lookup(reader, export, state),
        OP_LOOKUPP => Ok(handle_lookupp(export, state)),
        OP_OPEN => handle_open(reader, generation, clients, export, state),
        OP_OPEN_CONFIRM => handle_open_confirm(reader, export),
        OP_PUTFH => handle_putfh(reader, generation, export, state),
        OP_PUTPUBFH | OP_PUTROOTFH => {
            state.current = Some(export.root());
            Ok((NFS4_OK, op_status(op, NFS4_OK).into_inner()))
        },
        OP_READ => handle_read_op(reader, export, state),
        OP_READDIR => handle_readdir_op(reader, generation, export, state),
        OP_READLINK => Ok(handle_readlink(export, state.current)),
        OP_RENEW => handle_renew(reader, clients, export),
        OP_RESTOREFH => Ok(handle_restorefh(state)),
        OP_SAVEFH => Ok(handle_savefh(state)),
        OP_SECINFO => handle_secinfo(reader, export, state),
        OP_SETCLIENTID => handle_setclientid(reader, clients),
        OP_SETCLIENTID_CONFIRM => handle_setclientid_confirm(reader, clients),
        OP_VERIFY => handle_verify(reader),
        _ => Ok(error_reply(OP_ILLEGAL, Status::OpIllegal)),
    }
}

fn handle_access(
    reader: &mut XdrReader<'_>,
    export: &dyn ReadOnlyExport,
    state: &CompoundState,
) -> Result<(u32, Vec<u8>), XdrError> {
    let requested = AccessMask(reader.u32()?);
    let status = state.current.ok_or(Status::NoFileHandle).and_then(|id| {
        let attr = export.attr(id)?;
        Ok((
            AccessMask::READ_ONLY_SUPPORTED.raw(),
            requested.allowed_for(attr.kind).raw(),
        ))
    });
    Ok(encode_access(status))
}

fn handle_close(
    reader: &mut XdrReader<'_>,
    export: &dyn ReadOnlyExport,
) -> Result<(u32, Vec<u8>), XdrError> {
    let _seqid = reader.u32()?;
    let raw = reader.fixed_opaque(16)?;
    let status = StateId::from_wire(&raw).and_then(|stateid| export.close_state(stateid));
    match status {
        Ok(next) => {
            let mut res = op_status(OP_CLOSE, NFS4_OK);
            res.bytes(&next.to_wire());
            Ok((NFS4_OK, res.into_inner()))
        },
        Err(status) => Ok(error_reply(OP_CLOSE, status)),
    }
}

fn handle_getattr(
    reader: &mut XdrReader<'_>,
    generation: u64,
    export: &dyn ReadOnlyExport,
    state: &CompoundState,
) -> Result<(u32, Vec<u8>), XdrError> {
    let request = reader.bitmap()?;
    let status = state
        .current
        .ok_or(Status::NoFileHandle)
        .and_then(|id| export.attr(id))
        .map(|attr| encode_attrs(generation, &attr, &request));
    Ok(encode_getattr(status))
}

fn handle_getfh(
    generation: u64,
    export: &dyn ReadOnlyExport,
    state: &CompoundState,
) -> (u32, Vec<u8>) {
    let status = state
        .current
        .ok_or(Status::NoFileHandle)
        .and_then(|id| export.attr(id).map(|_| file_handle(generation, id)));
    encode_getfh(status)
}

fn handle_lookup(
    reader: &mut XdrReader<'_>,
    export: &dyn ReadOnlyExport,
    state: &mut CompoundState,
) -> Result<(u32, Vec<u8>), XdrError> {
    let raw = reader.string()?;
    state.events.push(format!("lookup={}", raw.escape_debug()));
    let status = match ComponentName::parse(&raw) {
        Ok(name) => match state.current {
            Some(parent) => export.lookup(parent, name.as_ref()).map(|child| {
                state.current = Some(child);
            }),
            None => Err(Status::NoFileHandle),
        },
        Err(_) => Err(Status::Invalid),
    };
    Ok((
        status_to_u32(status),
        op_status(OP_LOOKUP, status_to_u32(status)).into_inner(),
    ))
}

fn handle_lookupp(export: &dyn ReadOnlyExport, state: &mut CompoundState) -> (u32, Vec<u8>) {
    let status = match state.current {
        Some(id) => export.parent(id).map(|parent| {
            state.current = Some(parent);
        }),
        None => Err(Status::NoFileHandle),
    };
    (
        status_to_u32(status),
        op_status(OP_LOOKUPP, status_to_u32(status)).into_inner(),
    )
}

fn handle_open_confirm(
    reader: &mut XdrReader<'_>,
    export: &dyn ReadOnlyExport,
) -> Result<(u32, Vec<u8>), XdrError> {
    let raw = reader.fixed_opaque(16)?;
    let _seqid = reader.u32()?;
    let status = match StateId::from_wire(&raw) {
        Ok(stateid) => match export.validate_state(stateid) {
            Ok(()) => Status::NotSupported,
            Err(status) => status,
        },
        Err(status) => status,
    };
    Ok(error_reply(OP_OPEN_CONFIRM, status))
}

fn handle_putfh(
    reader: &mut XdrReader<'_>,
    generation: u64,
    export: &dyn ReadOnlyExport,
    state: &mut CompoundState,
) -> Result<(u32, Vec<u8>), XdrError> {
    let fh = reader.opaque()?;
    let status = decode_file_handle(generation, &fh)
        .and_then(|id| export.attr(id).map(|_| id))
        .map(|id| {
            state.current = Some(id);
        });
    Ok((
        status_to_u32(status),
        op_status(OP_PUTFH, status_to_u32(status)).into_inner(),
    ))
}

fn handle_read_op(
    reader: &mut XdrReader<'_>,
    export: &dyn ReadOnlyExport,
    state: &mut CompoundState,
) -> Result<(u32, Vec<u8>), XdrError> {
    let raw = reader.fixed_opaque(16)?;
    let offset = reader.u64()?;
    let count = reader.u32()?;
    Ok(match StateId::from_wire(&raw) {
        Ok(stateid) => handle_read(
            export,
            state.current,
            stateid,
            offset,
            count,
            &mut state.events,
        ),
        Err(status) => error_reply(OP_READ, status),
    })
}

fn handle_readdir_op(
    reader: &mut XdrReader<'_>,
    generation: u64,
    export: &dyn ReadOnlyExport,
    state: &CompoundState,
) -> Result<(u32, Vec<u8>), XdrError> {
    let cookie = reader.u64()?;
    let verifier = reader.fixed_opaque(8)?;
    let _dircount = reader.u32()?;
    let maxcount = reader.u32()?;
    let attrs = reader.bitmap()?;
    Ok(handle_readdir(
        export,
        generation,
        state.current,
        cookie,
        &verifier,
        maxcount,
        &attrs,
    ))
}

fn handle_renew(
    reader: &mut XdrReader<'_>,
    clients: &ClientTable,
    export: &dyn ReadOnlyExport,
) -> Result<(u32, Vec<u8>), XdrError> {
    let clientid = reader.u64()?;
    if clients.is_confirmed(clientid) {
        match export.renew_client(clientid) {
            Ok(()) => Ok((NFS4_OK, op_status(OP_RENEW, NFS4_OK).into_inner())),
            Err(status) => Ok(error_reply(OP_RENEW, status)),
        }
    } else {
        Ok(error_reply(OP_RENEW, Status::StaleClientId))
    }
}

fn handle_restorefh(state: &mut CompoundState) -> (u32, Vec<u8>) {
    let status = match state.saved {
        Some(saved) => {
            state.current = Some(saved);
            NFS4_OK
        },
        None => Status::NoFileHandle.wire(),
    };
    (status, op_status(OP_RESTOREFH, status).into_inner())
}

fn handle_savefh(state: &mut CompoundState) -> (u32, Vec<u8>) {
    let status = match state.current {
        Some(current) => {
            state.saved = Some(current);
            NFS4_OK
        },
        None => Status::NoFileHandle.wire(),
    };
    (status, op_status(OP_SAVEFH, status).into_inner())
}

fn handle_secinfo(
    reader: &mut XdrReader<'_>,
    export: &dyn ReadOnlyExport,
    state: &CompoundState,
) -> Result<(u32, Vec<u8>), XdrError> {
    let raw = reader.string()?;
    let status = match ComponentName::parse(&raw) {
        Ok(name) => match state.current {
            Some(parent) => export.lookup(parent, name.as_ref()).map(|_| ()),
            None => Err(Status::NoFileHandle),
        },
        Err(_) => Err(Status::Invalid),
    };
    if let Err(status) = status {
        return Ok(error_reply(OP_SECINFO, status));
    }
    let mut res = op_status(OP_SECINFO, NFS4_OK);
    res.u32(2);
    res.u32(AUTH_SYS);
    res.u32(AUTH_NONE);
    Ok((NFS4_OK, res.into_inner()))
}

// SETCLIENTID is intentionally a deterministic stub: every caller is handed
// back the same process-generation-derived client id and a fixed confirm
// verifier. The client's own verifier, owner, and callback advertisement are
// read and discarded. This makes two simultaneous clients indistinguishable
// at the protocol level, which is only acceptable because `start_server`
// (see `server.rs`) refuses non-loopback binds. If the server is ever
// exposed beyond the local host, this handler must grow real per-client
// state with verifier-based identity reuse and conflict handling.
fn handle_setclientid(
    reader: &mut XdrReader<'_>,
    clients: &ClientTable,
) -> Result<(u32, Vec<u8>), XdrError> {
    let verifier = reader.fixed_opaque(8)?;
    let owner = reader.opaque()?;
    let _callback_program = reader.u32()?;
    let _callback_netid = reader.string()?;
    let _callback_addr = reader.string()?;
    let _callback_ident = reader.u32()?;
    let mut verifier_bytes = [0_u8; 8];
    verifier_bytes.copy_from_slice(&verifier);
    let assignment = clients.set_clientid(verifier_bytes, owner);
    let mut res = op_status(OP_SETCLIENTID, NFS4_OK);
    res.u64(assignment.clientid);
    res.bytes(&assignment.confirm);
    Ok((NFS4_OK, res.into_inner()))
}

fn handle_setclientid_confirm(
    reader: &mut XdrReader<'_>,
    clients: &ClientTable,
) -> Result<(u32, Vec<u8>), XdrError> {
    let clientid = reader.u64()?;
    let verifier = reader.fixed_opaque(8)?;
    let status = clients
        .confirm(clientid, &verifier)
        .map_or_else(Status::wire, |()| NFS4_OK);
    Ok((
        status,
        op_status(OP_SETCLIENTID_CONFIRM, status).into_inner(),
    ))
}

fn handle_verify(reader: &mut XdrReader<'_>) -> Result<(u32, Vec<u8>), XdrError> {
    let _ = reader.fattr()?;
    Ok(error_reply(OP_VERIFY, Status::NotSupported))
}

pub(crate) fn handle_open(
    reader: &mut XdrReader<'_>,
    generation: u64,
    clients: &ClientTable,
    export: &dyn ReadOnlyExport,
    state: &mut CompoundState,
) -> Result<(u32, Vec<u8>), XdrError> {
    let _seqid = reader.u32()?;
    let share_access = ShareAccess(reader.u32()?);
    let share_deny = ShareDeny(reader.u32()?);
    let owner_clientid = reader.u64()?;
    let _owner = reader.opaque()?;
    if !clients.is_confirmed(owner_clientid) {
        return Ok(error_reply(OP_OPEN, Status::StaleClientId));
    }
    let open_type = OpenType::read(reader)?;
    if open_type == OpenType::Unsupported {
        return Ok(error_reply(OP_OPEN, Status::ReadOnlyFs));
    }
    let claim_type = ClaimType::from_wire(reader.u32()?);
    let target = match claim_type {
        ClaimType::Null => {
            let raw = reader.string()?;
            state.events.push(format!(
                "open={} share_access={} opentype={open_type:?}",
                raw.escape_debug(),
                share_access.raw()
            ));
            match ComponentName::parse(&raw) {
                Ok(name) => match state.current {
                    Some(parent) => export.lookup(parent, name.as_ref()),
                    None => Err(Status::NoFileHandle),
                },
                Err(_) => Err(Status::Invalid),
            }
        },
        ClaimType::Fh => state.current.ok_or(Status::NoFileHandle),
        ClaimType::Previous => {
            let _delegate_type = reader.u32()?;
            Err(Status::NotSupported)
        },
        ClaimType::DelegateCur => {
            let _stateid = reader.fixed_opaque(16)?;
            let _file = reader.string()?;
            Err(Status::NotSupported)
        },
        ClaimType::DelegatePrev => {
            let _file = reader.string()?;
            Err(Status::NotSupported)
        },
        ClaimType::Unsupported => Err(Status::NotSupported),
    };

    if !share_access.is_valid() {
        return Ok(error_reply(OP_OPEN, Status::Invalid));
    }
    if open_type.is_create() || share_access.allows_write() {
        return Ok(error_reply(OP_OPEN, Status::ReadOnlyFs));
    }
    if !share_access.allows_read() {
        return Ok(error_reply(OP_OPEN, Status::Invalid));
    }
    if !share_deny.is_none() {
        return Ok(error_reply(OP_OPEN, Status::NotSupported));
    }

    let id = match target {
        Ok(id) => id,
        Err(status) => return Ok(error_reply(OP_OPEN, status)),
    };
    let attr = match export.attr(id) {
        Ok(attr) => attr,
        Err(status) => return Ok(error_reply(OP_OPEN, status)),
    };
    if attr.kind == NodeKind::Directory {
        return Ok(error_reply(OP_OPEN, Status::IsDir));
    }
    if attr.kind == NodeKind::Symlink {
        return Ok(error_reply(OP_OPEN, Status::Symlink));
    }

    let open = match export.open_state(generation, id, owner_clientid, share_access.raw()) {
        Ok(open) => open,
        Err(status) => return Ok(error_reply(OP_OPEN, status)),
    };
    state
        .events
        .push(format!("open_materialized={} bytes", open.attr.size));
    state.current = Some(id);

    let mut res = op_status(OP_OPEN, NFS4_OK);
    res.bytes(&open.stateid.to_wire());
    // change_info4 { bool atomic; changeid4 before; changeid4 after } — for a
    // read-only OPEN no parent directory change happens, so emit
    // (atomic=true, before=0, after=0) per RFC 7530 convention. The file's
    // change attribute belongs in GETATTR replies, not here.
    res.bool(true);
    res.u64(0);
    res.u64(0);
    // rflags=0: server requires no OPEN_CONFIRM round trip.
    res.u32(0);
    encode_bitmap(&mut res, &[]);
    res.u32(OPEN_DELEGATE_NONE);
    Ok((NFS4_OK, res.into_inner()))
}

pub(crate) fn handle_read(
    export: &dyn ReadOnlyExport,
    current: Option<u64>,
    stateid: StateId,
    offset: u64,
    count: u32,
    events: &mut Vec<String>,
) -> (u32, Vec<u8>) {
    let Some(id) = current else {
        events.push(format!(
            "read offset={offset} count={count} status={}",
            Status::NoFileHandle.wire()
        ));
        return error_reply(OP_READ, Status::NoFileHandle);
    };
    let attr = match export.attr(id) {
        Ok(attr) => attr,
        Err(status) => {
            events.push(format!(
                "read offset={offset} count={count} status={}",
                status.wire()
            ));
            return error_reply(OP_READ, status);
        },
    };
    if attr.kind == NodeKind::Directory {
        events.push(format!(
            "read offset={offset} count={count} status={}",
            Status::IsDir.wire()
        ));
        return error_reply(OP_READ, Status::IsDir);
    }
    if attr.kind == NodeKind::Symlink {
        events.push(format!(
            "read offset={offset} count={count} status={}",
            Status::Symlink.wire()
        ));
        return error_reply(OP_READ, Status::Symlink);
    }
    let open = match export.read_state(stateid, offset, count) {
        Ok(open) if open.id == id => open,
        Ok(_) => {
            events.push(format!(
                "read offset={offset} count={count} status={}",
                Status::BadStateId.wire()
            ));
            return error_reply(OP_READ, Status::BadStateId);
        },
        Err(status) => {
            events.push(format!(
                "read offset={offset} count={count} status={}",
                status.wire()
            ));
            return error_reply(OP_READ, status);
        },
    };
    events.push(format!(
        "read offset={offset} count={count} bytes={} eof={}",
        open.data.len(),
        open.eof
    ));
    let mut res = op_status(OP_READ, NFS4_OK);
    res.bool(open.eof);
    res.opaque(&open.data);
    (NFS4_OK, res.into_inner())
}

fn readdir_cookie_verifier(id: u64, change: u64, entries: &[DirEntry]) -> [u8; 8] {
    let mut hasher = DefaultHasher::new();
    id.hash(&mut hasher);
    change.hash(&mut hasher);
    entries.len().hash(&mut hasher);
    for entry in entries {
        entry.name.hash(&mut hasher);
        entry.id.hash(&mut hasher);
        entry.attr.change.hash(&mut hasher);
    }
    hasher.finish().to_be_bytes()
}

pub(crate) fn handle_readdir(
    export: &dyn ReadOnlyExport,
    generation: u64,
    current: Option<u64>,
    cookie: u64,
    verifier: &[u8],
    maxcount: u32,
    attrs: &[u32],
) -> (u32, Vec<u8>) {
    let Some(id) = current else {
        return error_reply(OP_READDIR, Status::NoFileHandle);
    };
    let dir_attr = match export.attr(id) {
        Ok(attr) => attr,
        Err(status) => return error_reply(OP_READDIR, status),
    };
    if dir_attr.kind != NodeKind::Directory {
        return error_reply(OP_READDIR, Status::NotDir);
    }
    let DirListing {
        mut entries,
        exhaustive: _,
    } = match export.readdir(id) {
        Ok(listing) => listing,
        Err(status) => return error_reply(OP_READDIR, status),
    };
    entries.sort_by(|left, right| left.name.cmp(&right.name).then(left.id.cmp(&right.id)));
    let cookie_verifier = readdir_cookie_verifier(id, dir_attr.change, &entries);
    let start = match cookie {
        0 => 0,
        1 | 2 => return error_reply(OP_READDIR, Status::BadCookie),
        _ if verifier != cookie_verifier => {
            return error_reply(OP_READDIR, Status::BadCookie);
        },
        cookie => {
            let idx = usize::try_from(cookie - 3).unwrap_or(usize::MAX);
            if idx >= entries.len() {
                return error_reply(OP_READDIR, Status::BadCookie);
            }
            idx + 1
        },
    };
    let mut res = op_status(OP_READDIR, NFS4_OK);
    res.bytes(&cookie_verifier);
    let maxcount = usize::try_from(maxcount).unwrap_or(usize::MAX);
    let trailer_len = 8; // final entry-present bool plus EOF bool.
    if res.len().saturating_add(trailer_len) > maxcount {
        return error_reply(OP_READDIR, Status::TooSmall);
    }
    let mut eof = true;
    for (emitted, (idx, entry)) in entries.iter().enumerate().skip(start).enumerate() {
        let mut encoded_entry = XdrWriter::new();
        encoded_entry.bool(true);
        encoded_entry.u64(u64::try_from(idx).expect("READDIR index exceeds u64") + 3);
        encoded_entry.string(&entry.name);
        encoded_entry.bytes(&encode_attrs(generation, &entry.attr, attrs));
        let encoded_entry = encoded_entry.into_inner();
        if res
            .len()
            .saturating_add(encoded_entry.len())
            .saturating_add(trailer_len)
            > maxcount
        {
            if emitted == 0 {
                return error_reply(OP_READDIR, Status::TooSmall);
            }
            eof = false;
            break;
        }
        res.bytes(&encoded_entry);
    }
    res.bool(false);
    res.bool(eof);
    (NFS4_OK, res.into_inner())
}

pub(crate) fn handle_readlink(export: &dyn ReadOnlyExport, current: Option<u64>) -> (u32, Vec<u8>) {
    let Some(id) = current else {
        return error_reply(OP_READLINK, Status::NoFileHandle);
    };
    match export.readlink(id) {
        Ok(target) => {
            let mut res = op_status(OP_READLINK, NFS4_OK);
            res.opaque(&target);
            (NFS4_OK, res.into_inner())
        },
        Err(status) => error_reply(OP_READLINK, status),
    }
}

fn error_reply(op: u32, status: Status) -> (u32, Vec<u8>) {
    let wire = status.wire();
    (wire, op_status(op, wire).into_inner())
}
