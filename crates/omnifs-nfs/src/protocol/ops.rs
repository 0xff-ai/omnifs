use crate::export::{NfsNodeKind, ReadOnlyExport};
use crate::protocol::attrs::{
    encode_access, encode_attrs, encode_bitmap, encode_getattr, encode_getfh, op_status,
    status_to_u32,
};
use crate::protocol::compound::CompoundState;
use crate::protocol::consts::{
    ACCESS4_DELETE, ACCESS4_EXECUTE, ACCESS4_EXTEND, ACCESS4_LOOKUP, ACCESS4_MODIFY, ACCESS4_READ,
    AUTH_NONE, AUTH_SYS, CLAIM_DELEGATE_CUR, CLAIM_DELEGATE_PREV, CLAIM_FH, CLAIM_NULL,
    CLAIM_PREVIOUS, CLIENT_ID, EXCLUSIVE4, GUARDED4, NFS4_OK, NFS4ERR_BAD_COOKIE, NFS4ERR_INVAL,
    NFS4ERR_ISDIR, NFS4ERR_LOCK_NOTSUPP, NFS4ERR_NOFILEHANDLE, NFS4ERR_NOTDIR, NFS4ERR_NOTSUPP,
    NFS4ERR_OP_ILLEGAL, NFS4ERR_ROFS, NFS4ERR_STALE_CLIENTID, NFS4ERR_TOOSMALL, OP_ACCESS,
    OP_CLOSE, OP_COMMIT, OP_CREATE, OP_GETATTR, OP_GETFH, OP_ILLEGAL, OP_LINK, OP_LOCK, OP_LOCKT,
    OP_LOCKU, OP_LOOKUP, OP_LOOKUPP, OP_OPEN, OP_OPEN_CONFIRM, OP_OPEN_DOWNGRADE, OP_OPENATTR,
    OP_PUTFH, OP_PUTPUBFH, OP_PUTROOTFH, OP_READ, OP_READDIR, OP_READLINK, OP_RELEASE_LOCKOWNER,
    OP_REMOVE, OP_RENAME, OP_RENEW, OP_RESTOREFH, OP_SAVEFH, OP_SECINFO, OP_SETATTR,
    OP_SETCLIENTID, OP_SETCLIENTID_CONFIRM, OP_VERIFY, OP_WRITE, OPEN_DELEGATE_NONE,
    OPEN_MATERIALIZE_LIMIT_BYTES, OPEN4_SHARE_ACCESS_WRITE, READDIR_COOKIE_VERIFIER,
    SETCLIENTID_CONFIRM_VERIFIER, UNCHECKED4,
};
use crate::protocol::filehandle::{decode_file_handle, file_handle};
use crate::protocol::name::is_valid_component;
use crate::protocol::xdr::{XdrError, XdrReader, XdrWriter};

#[allow(clippy::too_many_lines)]
pub(crate) fn handle_op(
    op: u32,
    reader: &mut XdrReader<'_>,
    generation: u64,
    export: &dyn ReadOnlyExport,
    state: &mut CompoundState,
) -> Result<(u32, Vec<u8>), XdrError> {
    match op {
        OP_ACCESS => {
            let requested = reader.u32()?;
            let status = state.current.ok_or(NFS4ERR_NOFILEHANDLE).and_then(|id| {
                let attr = export.attr(id)?;
                let supported = ACCESS4_READ
                    | ACCESS4_LOOKUP
                    | ACCESS4_MODIFY
                    | ACCESS4_EXTEND
                    | ACCESS4_DELETE
                    | ACCESS4_EXECUTE;
                Ok((supported, requested & attr.kind.allowed_access()))
            });
            Ok(encode_access(status))
        },
        OP_CLOSE => {
            let _seqid = reader.u32()?;
            let stateid = reader.fixed_opaque(16)?;
            let mut res = op_status(OP_CLOSE, NFS4_OK);
            res.bytes(&stateid);
            Ok((NFS4_OK, res.into_inner()))
        },
        OP_COMMIT | OP_CREATE | OP_LINK | OP_REMOVE | OP_RENAME | OP_SETATTR | OP_WRITE => {
            Ok((NFS4ERR_ROFS, op_status(op, NFS4ERR_ROFS).into_inner()))
        },
        OP_GETATTR => {
            let request = reader.bitmap()?;
            let status = state
                .current
                .ok_or(NFS4ERR_NOFILEHANDLE)
                .and_then(|id| export.attr(id))
                .map(|attr| encode_attrs(generation, &attr, &request));
            Ok(encode_getattr(status))
        },
        OP_GETFH => {
            let status = state
                .current
                .ok_or(NFS4ERR_NOFILEHANDLE)
                .and_then(|id| export.attr(id).map(|_| file_handle(generation, id)));
            Ok(encode_getfh(status))
        },
        OP_LOCK | OP_LOCKT | OP_LOCKU | OP_RELEASE_LOCKOWNER => Ok((
            NFS4ERR_LOCK_NOTSUPP,
            op_status(op, NFS4ERR_LOCK_NOTSUPP).into_inner(),
        )),
        OP_OPENATTR | OP_OPEN_DOWNGRADE => {
            Ok((NFS4ERR_NOTSUPP, op_status(op, NFS4ERR_NOTSUPP).into_inner()))
        },
        OP_LOOKUP => {
            let name = reader.string()?;
            state.events.push(format!("lookup={}", name.escape_debug()));
            let status = if is_valid_component(&name) {
                match state.current {
                    Some(parent) => export.lookup(parent, &name).map(|child| {
                        state.current = Some(child);
                    }),
                    None => Err(NFS4ERR_NOFILEHANDLE),
                }
            } else {
                Err(NFS4ERR_INVAL)
            };
            Ok((
                status_to_u32(status),
                op_status(OP_LOOKUP, status_to_u32(status)).into_inner(),
            ))
        },
        OP_LOOKUPP => {
            let status = match state.current {
                Some(id) => export.parent(id).map(|parent| {
                    state.current = Some(parent);
                }),
                None => Err(NFS4ERR_NOFILEHANDLE),
            };
            Ok((
                status_to_u32(status),
                op_status(OP_LOOKUPP, status_to_u32(status)).into_inner(),
            ))
        },
        OP_OPEN => handle_open(reader, generation, export, state),
        OP_OPEN_CONFIRM => {
            let stateid = reader.fixed_opaque(16)?;
            let _seqid = reader.u32()?;
            let mut res = op_status(OP_OPEN_CONFIRM, NFS4_OK);
            res.bytes(&stateid);
            Ok((NFS4_OK, res.into_inner()))
        },
        OP_PUTFH => {
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
        },
        OP_PUTPUBFH | OP_PUTROOTFH => {
            state.current = Some(export.root());
            Ok((NFS4_OK, op_status(op, NFS4_OK).into_inner()))
        },
        OP_READ => {
            let _stateid = reader.fixed_opaque(16)?;
            let offset = reader.u64()?;
            let count = reader.u32()?;
            Ok(handle_read(
                export,
                state.current,
                offset,
                count,
                &mut state.events,
            ))
        },
        OP_READDIR => {
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
        },
        OP_READLINK => Ok(handle_readlink(export, state.current)),
        OP_RENEW => {
            let clientid = reader.u64()?;
            let status = if clientid == CLIENT_ID {
                NFS4_OK
            } else {
                NFS4ERR_STALE_CLIENTID
            };
            Ok((status, op_status(OP_RENEW, status).into_inner()))
        },
        OP_RESTOREFH => {
            let status = match state.saved {
                Some(saved) => {
                    state.current = Some(saved);
                    NFS4_OK
                },
                None => NFS4ERR_NOFILEHANDLE,
            };
            Ok((status, op_status(OP_RESTOREFH, status).into_inner()))
        },
        OP_SAVEFH => {
            let status = match state.current {
                Some(current) => {
                    state.saved = Some(current);
                    NFS4_OK
                },
                None => NFS4ERR_NOFILEHANDLE,
            };
            Ok((status, op_status(OP_SAVEFH, status).into_inner()))
        },
        OP_SECINFO => {
            let _name = reader.string()?;
            let mut res = op_status(OP_SECINFO, NFS4_OK);
            res.u32(2);
            res.u32(AUTH_SYS);
            res.u32(AUTH_NONE);
            Ok((NFS4_OK, res.into_inner()))
        },
        OP_SETCLIENTID => {
            let _verifier = reader.fixed_opaque(8)?;
            let _owner = reader.opaque()?;
            let _callback_program = reader.u32()?;
            let _callback_netid = reader.string()?;
            let _callback_addr = reader.string()?;
            let _callback_ident = reader.u32()?;
            let mut res = op_status(OP_SETCLIENTID, NFS4_OK);
            res.u64(CLIENT_ID);
            res.bytes(&SETCLIENTID_CONFIRM_VERIFIER);
            Ok((NFS4_OK, res.into_inner()))
        },
        OP_SETCLIENTID_CONFIRM => {
            let clientid = reader.u64()?;
            let verifier = reader.fixed_opaque(8)?;
            let status = if clientid == CLIENT_ID && verifier == SETCLIENTID_CONFIRM_VERIFIER {
                NFS4_OK
            } else {
                NFS4ERR_STALE_CLIENTID
            };
            Ok((
                status,
                op_status(OP_SETCLIENTID_CONFIRM, status).into_inner(),
            ))
        },
        OP_VERIFY => {
            let _ = reader.fattr()?;
            Ok((
                NFS4ERR_NOTSUPP,
                op_status(OP_VERIFY, NFS4ERR_NOTSUPP).into_inner(),
            ))
        },
        OP_ILLEGAL => Ok((
            NFS4ERR_OP_ILLEGAL,
            op_status(OP_ILLEGAL, NFS4ERR_OP_ILLEGAL).into_inner(),
        )),
        _ => Ok((NFS4ERR_NOTSUPP, op_status(op, NFS4ERR_NOTSUPP).into_inner())),
    }
}

pub(crate) fn handle_open(
    reader: &mut XdrReader<'_>,
    generation: u64,
    export: &dyn ReadOnlyExport,
    state: &mut CompoundState,
) -> Result<(u32, Vec<u8>), XdrError> {
    let seqid = reader.u32()?;
    let share_access = reader.u32()?;
    let _share_deny = reader.u32()?;
    let _owner_clientid = reader.u64()?;
    let _owner = reader.opaque()?;
    let opentype = reader.u32()?;
    if opentype == 1 {
        let createmode = reader.u32()?;
        match createmode {
            UNCHECKED4 | GUARDED4 => {
                let _ = reader.fattr()?;
            },
            EXCLUSIVE4 => {
                let _ = reader.fixed_opaque(8)?;
            },
            _ => return Ok((NFS4ERR_ROFS, op_status(OP_OPEN, NFS4ERR_ROFS).into_inner())),
        }
    }
    let claim_type = reader.u32()?;
    let target = match claim_type {
        CLAIM_NULL => {
            let name = reader.string()?;
            state.events.push(format!(
                "open={} share_access={share_access} opentype={opentype}",
                name.escape_debug()
            ));
            if is_valid_component(&name) {
                match state.current {
                    Some(parent) => export.lookup(parent, &name),
                    None => Err(NFS4ERR_NOFILEHANDLE),
                }
            } else {
                Err(NFS4ERR_INVAL)
            }
        },
        CLAIM_FH => state.current.ok_or(NFS4ERR_NOFILEHANDLE),
        CLAIM_PREVIOUS => {
            let _delegate_type = reader.u32()?;
            Err(NFS4ERR_NOTSUPP)
        },
        CLAIM_DELEGATE_CUR => {
            let _stateid = reader.fixed_opaque(16)?;
            let _file = reader.string()?;
            Err(NFS4ERR_NOTSUPP)
        },
        CLAIM_DELEGATE_PREV => {
            let _file = reader.string()?;
            Err(NFS4ERR_NOTSUPP)
        },
        _ => Err(NFS4ERR_NOTSUPP),
    };

    if opentype == 1 || share_access & OPEN4_SHARE_ACCESS_WRITE != 0 {
        return Ok((NFS4ERR_ROFS, op_status(OP_OPEN, NFS4ERR_ROFS).into_inner()));
    }

    let id = match target {
        Ok(id) => id,
        Err(status) => return Ok((status, op_status(OP_OPEN, status).into_inner())),
    };
    let mut attr = match export.attr(id) {
        Ok(attr) => attr,
        Err(status) => return Ok((status, op_status(OP_OPEN, status).into_inner())),
    };
    if attr.kind == NfsNodeKind::Directory {
        return Ok((
            NFS4ERR_ISDIR,
            op_status(OP_OPEN, NFS4ERR_ISDIR).into_inner(),
        ));
    }
    if attr.kind == NfsNodeKind::Symlink {
        return Ok((
            NFS4ERR_INVAL,
            op_status(OP_OPEN, NFS4ERR_INVAL).into_inner(),
        ));
    }

    match export.materialize_for_open(id, OPEN_MATERIALIZE_LIMIT_BYTES) {
        Ok(size) => {
            state.events.push(format!("open_materialized={size} bytes"));
            attr = match export.attr(id) {
                Ok(attr) => attr,
                Err(status) => return Ok((status, op_status(OP_OPEN, status).into_inner())),
            };
        },
        Err(status) => return Ok((status, op_status(OP_OPEN, status).into_inner())),
    }
    state.current = Some(id);

    let mut stateid = [0_u8; 16];
    stateid[..4].copy_from_slice(&seqid.to_be_bytes());
    stateid[4..12].copy_from_slice(&id.to_be_bytes());
    stateid[12..16].copy_from_slice(&generation.to_be_bytes()[4..]);

    let mut res = op_status(OP_OPEN, NFS4_OK);
    res.bytes(&stateid);
    res.bool(true);
    res.u64(attr.change);
    res.u64(attr.change);
    res.u32(0);
    encode_bitmap(&mut res, &[]);
    res.u32(OPEN_DELEGATE_NONE);
    Ok((NFS4_OK, res.into_inner()))
}

pub(crate) fn handle_read(
    export: &dyn ReadOnlyExport,
    current: Option<u64>,
    offset: u64,
    count: u32,
    events: &mut Vec<String>,
) -> (u32, Vec<u8>) {
    let Some(id) = current else {
        events.push(format!(
            "read offset={offset} count={count} status={NFS4ERR_NOFILEHANDLE}"
        ));
        return (
            NFS4ERR_NOFILEHANDLE,
            op_status(OP_READ, NFS4ERR_NOFILEHANDLE).into_inner(),
        );
    };
    let attr = match export.attr(id) {
        Ok(attr) => attr,
        Err(status) => {
            events.push(format!(
                "read offset={offset} count={count} status={status}"
            ));
            return (status, op_status(OP_READ, status).into_inner());
        },
    };
    if attr.kind == NfsNodeKind::Directory {
        events.push(format!(
            "read offset={offset} count={count} status={NFS4ERR_ISDIR}"
        ));
        return (
            NFS4ERR_ISDIR,
            op_status(OP_READ, NFS4ERR_ISDIR).into_inner(),
        );
    }
    let data = match export.read(id) {
        Ok(data) => data,
        Err(status) => {
            events.push(format!(
                "read offset={offset} count={count} status={status}"
            ));
            return (status, op_status(OP_READ, status).into_inner());
        },
    };
    let start = usize::try_from(offset)
        .unwrap_or(usize::MAX)
        .min(data.len());
    let end = start
        .saturating_add(usize::try_from(count).unwrap_or(usize::MAX))
        .min(data.len());
    let eof = end >= data.len();
    events.push(format!(
        "read offset={offset} count={count} data_len={} chunk={} eof={eof}",
        data.len(),
        end.saturating_sub(start)
    ));
    let mut res = op_status(OP_READ, NFS4_OK);
    res.bool(eof);
    res.opaque(&data[start..end]);
    (NFS4_OK, res.into_inner())
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
        return (
            NFS4ERR_NOFILEHANDLE,
            op_status(OP_READDIR, NFS4ERR_NOFILEHANDLE).into_inner(),
        );
    };
    let dir_attr = match export.attr(id) {
        Ok(attr) => attr,
        Err(status) => return (status, op_status(OP_READDIR, status).into_inner()),
    };
    if dir_attr.kind != NfsNodeKind::Directory {
        return (
            NFS4ERR_NOTDIR,
            op_status(OP_READDIR, NFS4ERR_NOTDIR).into_inner(),
        );
    }
    if cookie != 0 && verifier != READDIR_COOKIE_VERIFIER {
        return (
            NFS4ERR_BAD_COOKIE,
            op_status(OP_READDIR, NFS4ERR_BAD_COOKIE).into_inner(),
        );
    }
    let entries = match export.readdir(id) {
        Ok(entries) => entries,
        Err(status) => return (status, op_status(OP_READDIR, status).into_inner()),
    };
    let start = if cookie == 0 {
        0
    } else {
        let idx = usize::try_from(cookie.saturating_sub(3)).unwrap_or(usize::MAX);
        idx.saturating_add(1).min(entries.len())
    };
    let mut res = op_status(OP_READDIR, NFS4_OK);
    res.bytes(&READDIR_COOKIE_VERIFIER);
    let maxcount = usize::try_from(maxcount).unwrap_or(usize::MAX);
    let trailer_len = 8;
    if res.len().saturating_add(trailer_len) > maxcount {
        return (
            NFS4ERR_TOOSMALL,
            op_status(OP_READDIR, NFS4ERR_TOOSMALL).into_inner(),
        );
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
                return (
                    NFS4ERR_TOOSMALL,
                    op_status(OP_READDIR, NFS4ERR_TOOSMALL).into_inner(),
                );
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
        return (
            NFS4ERR_NOFILEHANDLE,
            op_status(OP_READLINK, NFS4ERR_NOFILEHANDLE).into_inner(),
        );
    };
    match export.readlink(id) {
        Ok(target) => {
            let mut res = op_status(OP_READLINK, NFS4_OK);
            res.opaque(&target);
            (NFS4_OK, res.into_inner())
        },
        Err(status) => (status, op_status(OP_READLINK, status).into_inner()),
    }
}
