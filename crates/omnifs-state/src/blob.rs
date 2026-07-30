//! Audited `SQLite` incremental BLOB calls.

#![allow(unsafe_code)]

use libsqlite3_sys::{
    SQLITE_OK, sqlite3, sqlite3_blob, sqlite3_blob_bytes, sqlite3_blob_close, sqlite3_blob_open,
    sqlite3_blob_read, sqlite3_blob_write,
};
use std::ffi::c_void;
use std::ptr::NonNull;

pub(crate) struct BlobHandle {
    handle: Option<NonNull<sqlite3_blob>>,
}

impl BlobHandle {
    pub(crate) fn open_read(
        database: NonNull<sqlite3>,
        table: &'static std::ffi::CStr,
        column: &'static std::ffi::CStr,
        row_id: i64,
    ) -> Result<Self, BlobError> {
        Self::open(database, table, column, row_id, false)
    }

    pub(crate) fn open_write(
        database: NonNull<sqlite3>,
        table: &'static std::ffi::CStr,
        column: &'static std::ffi::CStr,
        row_id: i64,
    ) -> Result<Self, BlobError> {
        Self::open(database, table, column, row_id, true)
    }

    fn open(
        database: NonNull<sqlite3>,
        table: &'static std::ffi::CStr,
        column: &'static std::ffi::CStr,
        row_id: i64,
        writable: bool,
    ) -> Result<Self, BlobError> {
        let mut handle = std::ptr::null_mut();
        // SAFETY: `database` is held by SQLx's `LockedSqliteHandle` for the
        // full lifetime of the returned BLOB. The schema, table, and column
        // pointers are static NUL-terminated strings. `handle` points to
        // writable storage and SQLite does not retain that pointer.
        let code = unsafe {
            sqlite3_blob_open(
                database.as_ptr(),
                c"main".as_ptr(),
                table.as_ptr(),
                column.as_ptr(),
                row_id,
                i32::from(writable),
                &raw mut handle,
            )
        };
        if code != SQLITE_OK {
            return Err(BlobError::sqlite("open", code));
        }
        let handle = NonNull::new(handle).ok_or(BlobError::NullHandle)?;
        Ok(Self {
            handle: Some(handle),
        })
    }

    pub(crate) fn read(&self, offset: usize, bytes: &mut [u8]) -> Result<(), BlobError> {
        let handle = self.handle.ok_or(BlobError::Closed)?;
        let length =
            i32::try_from(bytes.len()).map_err(|_| BlobError::ChunkTooLarge(bytes.len()))?;
        let offset = i32::try_from(offset).map_err(|_| BlobError::OffsetTooLarge(offset))?;
        // SAFETY: `handle` remains open, `bytes` points to at least `length`
        // writable bytes for this call, and SQLite does not retain the pointer.
        let code = unsafe {
            sqlite3_blob_read(
                handle.as_ptr(),
                bytes.as_mut_ptr().cast::<c_void>(),
                length,
                offset,
            )
        };
        if code == SQLITE_OK {
            Ok(())
        } else {
            Err(BlobError::sqlite("read", code))
        }
    }

    pub(crate) fn len(&self) -> Result<usize, BlobError> {
        let handle = self.handle.ok_or(BlobError::Closed)?;
        // SAFETY: `handle` is open and owned by `self`.
        let length = unsafe { sqlite3_blob_bytes(handle.as_ptr()) };
        usize::try_from(length).map_err(|_| BlobError::BadLength(length))
    }

    pub(crate) fn write(&mut self, offset: usize, bytes: &[u8]) -> Result<(), BlobError> {
        let handle = self.handle.ok_or(BlobError::Closed)?;
        let length =
            i32::try_from(bytes.len()).map_err(|_| BlobError::ChunkTooLarge(bytes.len()))?;
        let offset = i32::try_from(offset).map_err(|_| BlobError::OffsetTooLarge(offset))?;
        // SAFETY: `handle` remains open, `bytes` points to at least `length`
        // readable bytes for this call, and SQLite does not retain the pointer.
        // The caller checks that offset plus length stays within the fixed BLOB.
        let code = unsafe {
            sqlite3_blob_write(
                handle.as_ptr(),
                bytes.as_ptr().cast::<c_void>(),
                length,
                offset,
            )
        };
        if code == SQLITE_OK {
            Ok(())
        } else {
            Err(BlobError::sqlite("write", code))
        }
    }

    pub(crate) fn close(mut self) -> Result<(), BlobError> {
        self.close_inner()
    }

    fn close_inner(&mut self) -> Result<(), BlobError> {
        let Some(handle) = self.handle.take() else {
            return Ok(());
        };
        // SAFETY: `handle` is open, owned by `self`, and consumed exactly once.
        let code = unsafe { sqlite3_blob_close(handle.as_ptr()) };
        if code == SQLITE_OK {
            Ok(())
        } else {
            Err(BlobError::sqlite("close", code))
        }
    }
}

impl Drop for BlobHandle {
    fn drop(&mut self) {
        let _ = self.close_inner();
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum BlobError {
    #[error("SQLite returned a null incremental BLOB handle")]
    NullHandle,
    #[error("incremental BLOB handle is closed")]
    Closed,
    #[error("SQLite returned invalid BLOB length {0}")]
    BadLength(i32),
    #[error("incremental BLOB chunk is too large: {0} bytes")]
    ChunkTooLarge(usize),
    #[error("incremental BLOB offset is too large: {0}")]
    OffsetTooLarge(usize),
    #[error("SQLite incremental BLOB {operation} failed: {source}")]
    Sqlite {
        operation: &'static str,
        source: libsqlite3_sys::Error,
    },
}

impl BlobError {
    fn sqlite(operation: &'static str, code: i32) -> Self {
        Self::Sqlite {
            operation,
            source: libsqlite3_sys::Error::new(code),
        }
    }
}
