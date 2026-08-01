//! Column-level decoding shared by the hand-written `FromRow` impls.
//!
//! Domain types own their own `FromRow`; this only carries the primitives that
//! every one of them needs, so a stored digest or counter is validated in one
//! place instead of once per table.

use anyhow::Context as _;
use sqlx::Row as _;
use sqlx::sqlite::SqliteRow;
use std::num::NonZeroU64;

pub(crate) trait RowExt {
    /// Read a fixed-width digest column, proving its length.
    fn digest<const N: usize>(&self, column: &str) -> anyhow::Result<[u8; N]>;

    /// Read a positive counter column.
    fn counter(&self, column: &str) -> anyhow::Result<NonZeroU64>;

    /// Read a non-negative counter column.
    fn unsigned(&self, column: &str) -> anyhow::Result<u64>;

    fn text(&self, column: &str) -> anyhow::Result<String>;

    fn optional_text(&self, column: &str) -> anyhow::Result<Option<String>>;

    fn bytes(&self, column: &str) -> anyhow::Result<Vec<u8>>;

    fn optional_bytes(&self, column: &str) -> anyhow::Result<Option<Vec<u8>>>;
}

impl RowExt for SqliteRow {
    fn digest<const N: usize>(&self, column: &str) -> anyhow::Result<[u8; N]> {
        let bytes = self.bytes(column)?;
        let length = bytes.len();
        bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("stored `{column}` has {length} bytes; expected {N}"))
    }

    fn counter(&self, column: &str) -> anyhow::Result<NonZeroU64> {
        NonZeroU64::new(self.unsigned(column)?)
            .with_context(|| format!("stored `{column}` is zero"))
    }

    fn unsigned(&self, column: &str) -> anyhow::Result<u64> {
        let value: i64 = self
            .try_get(column)
            .with_context(|| format!("read column `{column}`"))?;
        u64::try_from(value).with_context(|| format!("stored `{column}` is negative"))
    }

    fn text(&self, column: &str) -> anyhow::Result<String> {
        self.try_get(column)
            .with_context(|| format!("read column `{column}`"))
    }

    fn optional_text(&self, column: &str) -> anyhow::Result<Option<String>> {
        self.try_get(column)
            .with_context(|| format!("read column `{column}`"))
    }

    fn bytes(&self, column: &str) -> anyhow::Result<Vec<u8>> {
        self.try_get(column)
            .with_context(|| format!("read column `{column}`"))
    }

    fn optional_bytes(&self, column: &str) -> anyhow::Result<Option<Vec<u8>>> {
        self.try_get(column)
            .with_context(|| format!("read column `{column}`"))
    }
}

/// Widen a domain counter into the signed integer `SQLite` stores.
pub(crate) fn sql_int(value: u64, what: &str) -> anyhow::Result<i64> {
    i64::try_from(value).with_context(|| format!("{what} exceeds SQLite integer range"))
}

/// Adapt a domain decode failure into the error `FromRow` must return.
pub(crate) fn decode_error(error: anyhow::Error) -> sqlx::Error {
    sqlx::Error::Decode(error.into())
}
