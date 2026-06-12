//! Internal machinery: the handle table behind streamed/ranged file reads.
//!
//! Providers never touch this directly. The `#[omnifs_sdk::provider]` macro
//! owns one `RangeReaders` per provider in a thread-local and wires it to
//! the WIT surface: `open-file` allocates a handle for the
//! [`RangeReader`] the route produced, `read-chunk` looks the handle up and
//! drives the reader, and `close-file` removes it. Handles are
//! `NonZeroU64` so zero stays an invalid value on the wire; the glue
//! rejects a zero handle before reaching this table.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::rc::Rc;

use crate::handler::RangeReader;

/// Open ranged-read handles for one provider instance. Single-threaded by
/// construction, so `Cell`/`RefCell` suffice.
pub struct RangeReaders {
    next: Cell<NonZeroU64>,
    readers: RefCell<BTreeMap<NonZeroU64, Rc<dyn RangeReader>>>,
}

impl RangeReaders {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            next: Cell::new(NonZeroU64::MIN),
            readers: RefCell::new(BTreeMap::new()),
        }
    }

    /// Insert a reader under the next free handle. Allocation scans forward
    /// with wraparound from a monotonic cursor, so handles are not reused
    /// until the space wraps; `None` (every handle simultaneously occupied)
    /// is unreachable in practice and surfaces as an internal error in the
    /// glue.
    pub fn allocate(&self, reader: Rc<dyn RangeReader>) -> Option<NonZeroU64> {
        let mut readers = self.readers.borrow_mut();
        let start = self.next.get();
        let mut handle = start;
        loop {
            match readers.entry(handle) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(reader);
                    let next_handle =
                        NonZeroU64::new(handle.get().wrapping_add(1)).unwrap_or(NonZeroU64::MIN);
                    self.next.set(next_handle);
                    return Some(handle);
                },
                std::collections::btree_map::Entry::Occupied(_) => {},
            }
            let next = NonZeroU64::new(handle.get().wrapping_add(1)).unwrap_or(NonZeroU64::MIN);
            handle = next;
            if handle == start {
                return None;
            }
        }
    }

    /// Look up a reader, cloning the `Rc` so the table borrow is released
    /// before the read future runs; a `read-chunk` await must not hold the
    /// table borrowed.
    pub fn get(&self, handle: NonZeroU64) -> Option<Rc<dyn RangeReader>> {
        self.readers.borrow().get(&handle).cloned()
    }

    /// Drop a handle on `close-file`. Removing an unknown handle is a no-op
    /// because close must be idempotent.
    pub fn remove(&self, handle: NonZeroU64) {
        self.readers.borrow_mut().remove(&handle);
    }

    /// Drop every open range handle and reset the allocator. Called on provider
    /// shutdown so readers left open by an aborted open/read/close sequence (or
    /// an open whose return the host rejected) do not outlive the instance.
    pub fn clear(&self) {
        self.readers.borrow_mut().clear();
        self.next.set(NonZeroU64::MIN);
    }
}

impl Default for RangeReaders {
    fn default() -> Self {
        Self::new()
    }
}
