use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::rc::Rc;

use crate::handler::RangeReader;

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

    pub fn get(&self, handle: NonZeroU64) -> Option<Rc<dyn RangeReader>> {
        self.readers.borrow().get(&handle).cloned()
    }

    pub fn remove(&self, handle: NonZeroU64) {
        self.readers.borrow_mut().remove(&handle);
    }
}

impl Default for RangeReaders {
    fn default() -> Self {
        Self::new()
    }
}
