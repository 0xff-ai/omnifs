//! Client-side wire cache for the Omnifs VFS wire protocol.
//!
//! [`WireNamespace`](crate::WireNamespace) uses it only to reduce transport
//! round trips; cacheability and invalidation semantics remain engine-owned.
//!
//! Every uncached out-of-process filesystem op crosses the wire as one
//! round-trip. Measurements identified per-op serialization as the dominant
//! overhead, so this cache reduces round trips with two mechanisms, both keyed
//! strictly off the engine-decided [`Attrs::ttl`]:
//!
//! - **Answer memo.** A bounded per-node memo of `getattr`/`getattr_exact`
//!   answers (keyed [`NodeId`]) and `lookup` answers (keyed `(parent, name)`).
//!   It is filled from every answer that carries `ttl > 0`, including
//!   opportunistically from every [`DirEntry`](omnifs_engine::DirEntry) a
//!   `readdir` page carries, so a directory walk's per-child stat chatter (the
//!   NFS flatten path issues one `getattr_exact` per child) is served locally
//!   from the listing that already named every child. `ttl == 0` answers (live,
//!   dynamic, or unknown-size nodes) are never memoized.
//!
//! - **Read windows.** For a read on a node whose last-known attrs are `ttl > 0`
//!   (stable, exact size), a small read fetches an aligned window (2 MiB) once
//!   and serves this and subsequent in-window reads from the buffer, slicing
//!   exactly as the server would. Reads on `ttl == 0` nodes pass through
//!   untouched, byte-for-byte identical.
//!
//! # Consistency
//!
//! Protocol caches are advisory, epoch-guarded, and fed by at-least-once events.
//! Any `ttl > 0` answer is cacheable because the engine has already decided it
//! cannot change without an invalidation. Any
//! [`NsEvent`] naming a node drops that node's memoized attrs, its cached window,
//! and every `lookup` entry whose parent or child is that node. The VFS wire client
//! applies the event to this cache before it re-broadcasts it, so a subscriber
//! that observes an event is guaranteed the stale answer is already gone.
//!
//! # Bounds
//!
//! The answer memo is two generation-swept maps (attrs, lookups). Each keeps a
//! young and an old generation; an insert past the young cap drops the old
//! generation wholesale and rotates young into old, so eviction is an O(1)
//! whole-generation drop rather than a scan, with peak residency about twice the
//! young cap. Windows are a 4-node LRU (8 MiB at 2 MiB each).

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use omnifs_engine::{Attrs, DirEntry, NodeAnswer, NodeId, NsEvent, ReadAnswer};

/// Window granularity: a small read fetches this many bytes on an aligned
/// boundary and serves subsequent in-window reads from the buffer.
pub(crate) const WINDOW_BYTES: u64 = 2 * 1024 * 1024;
/// Windows cached across nodes (LRU). Four 2 MiB windows cap window memory at
/// 8 MiB, the configured ceiling.
const MAX_WINDOWS: usize = 4;
/// Young-generation cap per memo. Peak residency is about twice this across the
/// young and old generations; a 32k young cap holds roughly 64k entries.
const MEMO_YOUNG_CAP: usize = 32 * 1024;

/// A memoized value with an optional expiry. `None` is effectively infinite: the
/// engine's stable-entry policy emits a `u32::MAX`-second TTL, so any finite
/// deadline it produces is honored while an effectively-infinite one never
/// expires on time (only an invalidation event drops it).
struct MemoEntry<T> {
    value: T,
    deadline: Option<Instant>,
}

impl<T> MemoEntry<T> {
    fn new(value: T, ttl: Duration) -> Self {
        Self {
            value,
            // A TTL large enough to overflow the clock is treated as infinite.
            deadline: Instant::now().checked_add(ttl),
        }
    }

    fn live(&self, now: Instant) -> bool {
        self.deadline.is_none_or(|deadline| now < deadline)
    }
}

/// A two-generation swept map. Inserts land in `young`; when it fills, `old` is
/// dropped and `young` rotates into it. A lookup checks `young` then `old`
/// without promotion (a walk touches each entry once, so promotion buys
/// nothing). Eviction is a whole-generation drop, never a scan.
struct GenMap<K, V> {
    young: HashMap<K, V>,
    old: HashMap<K, V>,
    young_cap: usize,
}

impl<K: std::hash::Hash + Eq, V> GenMap<K, V> {
    fn new(young_cap: usize) -> Self {
        Self {
            young: HashMap::new(),
            old: HashMap::new(),
            young_cap,
        }
    }

    fn get(&self, key: &K) -> Option<&V> {
        self.young.get(key).or_else(|| self.old.get(key))
    }

    fn insert(&mut self, key: K, value: V) {
        if self.young.len() >= self.young_cap {
            self.old = std::mem::take(&mut self.young);
        }
        self.young.insert(key, value);
    }

    fn remove(&mut self, key: &K) {
        self.young.remove(key);
        self.old.remove(key);
    }
}

/// One generation of the lookup memo: the `(parent, name) -> answer` entries plus
/// the reverse indices that make node-scoped invalidation O(affected) instead of
/// a scan. `by_parent` drops every child of an invalidated directory; `by_child`
/// drops every name that resolved to an invalidated node.
#[derive(Default)]
struct LookupGen {
    entries: HashMap<(NodeId, String), MemoEntry<NodeAnswer>>,
    by_parent: HashMap<NodeId, HashSet<String>>,
    by_child: HashMap<NodeId, HashSet<(NodeId, String)>>,
}

impl LookupGen {
    fn insert(&mut self, parent: NodeId, name: String, entry: MemoEntry<NodeAnswer>) {
        let child = entry.value.node;
        self.by_parent
            .entry(parent)
            .or_default()
            .insert(name.clone());
        self.by_child
            .entry(child)
            .or_default()
            .insert((parent, name.clone()));
        self.entries.insert((parent, name), entry);
    }

    fn get(&self, parent: NodeId, name: &str) -> Option<&MemoEntry<NodeAnswer>> {
        self.entries.get(&(parent, name.to_string()))
    }

    /// Drop every entry whose parent or child is `node`.
    fn invalidate(&mut self, node: NodeId) {
        if let Some(names) = self.by_parent.remove(&node) {
            for name in names {
                if let Some(entry) = self.entries.remove(&(node, name.clone())) {
                    let child = entry.value.node;
                    if let Some(keys) = self.by_child.get_mut(&child) {
                        keys.remove(&(node, name));
                        if keys.is_empty() {
                            self.by_child.remove(&child);
                        }
                    }
                }
            }
        }
        if let Some(keys) = self.by_child.remove(&node) {
            for (parent, name) in keys {
                self.entries.remove(&(parent, name.clone()));
                if let Some(names) = self.by_parent.get_mut(&parent) {
                    names.remove(&name);
                    if names.is_empty() {
                        self.by_parent.remove(&parent);
                    }
                }
            }
        }
    }
}

/// The generation-swept lookup memo. Mirrors [`GenMap`] but carries the reverse
/// indices per generation so an invalidation reaches both.
struct LookupMemo {
    young: LookupGen,
    old: LookupGen,
    young_cap: usize,
}

impl LookupMemo {
    fn new(young_cap: usize) -> Self {
        Self {
            young: LookupGen::default(),
            old: LookupGen::default(),
            young_cap,
        }
    }

    fn insert(&mut self, parent: NodeId, name: String, entry: MemoEntry<NodeAnswer>) {
        if self.young.entries.len() >= self.young_cap {
            self.old = std::mem::take(&mut self.young);
        }
        self.young.insert(parent, name, entry);
    }

    fn get(&self, parent: NodeId, name: &str) -> Option<&MemoEntry<NodeAnswer>> {
        self.young
            .get(parent, name)
            .or_else(|| self.old.get(parent, name))
    }

    fn invalidate(&mut self, node: NodeId) {
        self.young.invalidate(node);
        self.old.invalidate(node);
    }
}

/// A cached read window: the bytes at `[start, start + bytes.len())` plus the
/// attrs the fetch returned. `attrs` are the exact, stable attrs of a `ttl > 0`
/// node, so every in-window slice reports them (with `eof` recomputed per slice).
struct Window {
    start: u64,
    bytes: Vec<u8>,
    attrs: Attrs,
}

/// A tiny node-keyed LRU of read windows, capped at [`MAX_WINDOWS`].
struct WindowCache {
    windows: HashMap<NodeId, Window>,
    /// Least-recently-used first; the back is most recent.
    order: Vec<NodeId>,
}

impl WindowCache {
    fn new() -> Self {
        Self {
            windows: HashMap::new(),
            order: Vec::new(),
        }
    }

    fn touch(&mut self, node: NodeId) {
        if let Some(pos) = self.order.iter().position(|n| *n == node) {
            self.order.remove(pos);
        }
        self.order.push(node);
    }

    fn insert(&mut self, node: NodeId, window: Window) {
        if !self.windows.contains_key(&node) && self.windows.len() >= MAX_WINDOWS {
            // Evict the least-recently-used node that still has a window.
            while let Some(victim) = self.order.first().copied() {
                self.order.remove(0);
                if self.windows.remove(&victim).is_some() {
                    break;
                }
            }
        }
        self.windows.insert(node, window);
        self.touch(node);
    }

    fn remove(&mut self, node: NodeId) {
        self.windows.remove(&node);
        if let Some(pos) = self.order.iter().position(|n| *n == node) {
            self.order.remove(pos);
        }
    }

    /// Serve `[offset, offset + len)` from the node's window if it covers
    /// `offset`, slicing exactly as the server would: at most `len` bytes,
    /// truncated at the buffer end, with `eof` recomputed against `known_size`.
    fn slice(
        &mut self,
        node: NodeId,
        offset: u64,
        len: u32,
        known_size: u64,
    ) -> Option<ReadAnswer> {
        let window = self.windows.get(&node)?;
        if offset < window.start {
            return None;
        }
        let rel = usize::try_from(offset - window.start).ok()?;
        if rel >= window.bytes.len() {
            return None;
        }
        let end = rel.saturating_add(len as usize).min(window.bytes.len());
        let bytes = window.bytes[rel..end].to_vec();
        let eof = offset + bytes.len() as u64 >= known_size;
        let attrs = window.attrs.clone();
        self.touch(node);
        Some(ReadAnswer { bytes, eof, attrs })
    }
}

/// The mutable cache state, guarded by one mutex. No lock is held across an await
/// in the client: a caller reads or fills the cache in one synchronous critical
/// section and does its wire I/O outside the lock.
struct Inner {
    attrs: GenMap<NodeId, MemoEntry<Attrs>>,
    lookups: LookupMemo,
    windows: WindowCache,
    /// Nodes with a window fetch in flight, so a concurrent read on the same node
    /// passes through directly instead of launching a second fetch.
    window_inflight: HashSet<NodeId>,
}

impl Inner {
    fn insert_lookup(&mut self, parent: NodeId, name: String, answer: NodeAnswer) {
        let ttl = answer.attrs.ttl;
        self.attrs
            .insert(answer.node, MemoEntry::new(answer.attrs.clone(), ttl));
        self.lookups
            .insert(parent, name, MemoEntry::new(answer, ttl));
    }
}

/// The client-side batching cache. Cheap clone-out reads under a single mutex;
/// shared between the [`WireNamespace`](crate::WireNamespace) call paths and the
/// connection manager (which applies invalidation events to it).
pub(crate) struct WireCache {
    inner: Mutex<Inner>,
}

impl WireCache {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                attrs: GenMap::new(MEMO_YOUNG_CAP),
                lookups: LookupMemo::new(MEMO_YOUNG_CAP),
                windows: WindowCache::new(),
                window_inflight: HashSet::new(),
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Drop every cached answer and window. Called on a reattach: a reconnect
    /// onto a restarted daemon renumbers every [`NodeId`], so all memoized
    /// answers keyed by the old numbering are meaningless.
    pub(crate) fn clear(&self) {
        let mut inner = self.lock();
        inner.attrs = GenMap::new(MEMO_YOUNG_CAP);
        inner.lookups = LookupMemo::new(MEMO_YOUNG_CAP);
        inner.windows = WindowCache::new();
        inner.window_inflight.clear();
    }

    // --- answer memo --------------------------------------------------------

    /// The memoized attrs of `node`, if a live `ttl > 0` entry is present. Serves
    /// both `getattr` and `getattr_exact`: a `ttl > 0` node has an exact, stable
    /// size, which is exactly what `getattr_exact` returns without a probe.
    pub(crate) fn attrs(&self, node: NodeId) -> Option<Attrs> {
        let now = Instant::now();
        let inner = self.lock();
        inner
            .attrs
            .get(&node)
            .filter(|entry| entry.live(now))
            .map(|entry| entry.value.clone())
    }

    /// The memoized answer for `(parent, name)`, if a live `ttl > 0` entry is
    /// present.
    pub(crate) fn lookup(&self, parent: NodeId, name: &str) -> Option<NodeAnswer> {
        let now = Instant::now();
        let inner = self.lock();
        inner
            .lookups
            .get(parent, name)
            .filter(|entry| entry.live(now))
            .map(|entry| entry.value.clone())
    }

    /// Memoize an attrs answer for `node`, if it is cacheable (`ttl > 0`).
    pub(crate) fn put_attrs(&self, node: NodeId, attrs: &Attrs) {
        if attrs.ttl.is_zero() {
            return;
        }
        let mut inner = self.lock();
        inner
            .attrs
            .insert(node, MemoEntry::new(attrs.clone(), attrs.ttl));
    }

    /// Memoize a lookup answer, if it is cacheable (`ttl > 0`). The child's attrs
    /// are seeded into the attr memo too, so a follow-up stat on the resolved
    /// node is served locally.
    pub(crate) fn put_lookup(&self, parent: NodeId, name: &str, answer: &NodeAnswer) {
        if answer.attrs.ttl.is_zero() {
            return;
        }
        self.lock()
            .insert_lookup(parent, name.to_string(), answer.clone());
    }

    /// Seed the memo from a readdir page's children: every `ttl > 0` entry
    /// contributes both an attr memo entry (keyed by its node) and a lookup memo
    /// entry (keyed by the listed parent and name). This is what lets a walk's
    /// per-child stat chatter resolve locally against the listing that named it.
    pub(crate) fn seed_dir_entries(&self, parent: NodeId, entries: &[DirEntry]) {
        let mut inner = self.lock();
        for entry in entries {
            if entry.attrs.ttl.is_zero() {
                continue;
            }
            let answer = NodeAnswer {
                node: entry.node,
                attrs: entry.attrs.clone(),
                kind: entry.kind.clone(),
            };
            inner.insert_lookup(parent, entry.name.clone(), answer);
        }
    }

    /// Drop every cached answer, window, and lookup naming `node`. Applied for
    /// both [`NsEvent`] variants: an attrs change and a subtree invalidation each
    /// mean the node's cached state is stale.
    pub(crate) fn apply_event(&self, event: &NsEvent) {
        let node = match event {
            NsEvent::InvalidateSubtree { node, .. } | NsEvent::AttrsChanged { node, .. } => *node,
        };
        let mut inner = self.lock();
        inner.attrs.remove(&node);
        inner.windows.remove(node);
        inner.lookups.invalidate(node);
    }

    // --- read windows -------------------------------------------------------

    /// The exact size of `node` if the attr memo holds a live `ttl > 0` entry.
    /// `Some` gates the read-window path; `None` means read straight through.
    pub(crate) fn known_size(&self, node: NodeId) -> Option<u64> {
        let now = Instant::now();
        let inner = self.lock();
        inner
            .attrs
            .get(&node)
            .filter(|entry| entry.live(now))
            .map(|entry| entry.value.size)
    }

    /// Serve `[offset, offset + len)` from the node's cached window, if covered.
    pub(crate) fn window_slice(
        &self,
        node: NodeId,
        offset: u64,
        len: u32,
        known_size: u64,
    ) -> Option<ReadAnswer> {
        let mut inner = self.lock();
        inner.windows.slice(node, offset, len, known_size)
    }

    /// Claim the sole in-flight window fetch for `node`. Returns `false` when a
    /// fetch is already outstanding, so the caller reads straight through instead
    /// of launching a duplicate.
    pub(crate) fn try_begin_window(&self, node: NodeId) -> bool {
        self.lock().window_inflight.insert(node)
    }

    /// Release an in-flight claim without storing a window (the fetch failed).
    pub(crate) fn abort_window(&self, node: NodeId) {
        self.lock().window_inflight.remove(&node);
    }

    /// Store the freshly fetched window, release the in-flight claim, and slice
    /// the caller's `[offset, offset + len)` out of it in the same critical
    /// section. An empty fetch (a read at or past EOF) stores nothing.
    pub(crate) fn finish_window(
        &self,
        node: NodeId,
        start: u64,
        answer: ReadAnswer,
        offset: u64,
        len: u32,
        known_size: u64,
    ) -> ReadAnswer {
        let mut inner = self.lock();
        inner.window_inflight.remove(&node);
        if answer.bytes.is_empty() {
            return answer;
        }
        let attrs = answer.attrs.clone();
        inner.windows.insert(
            node,
            Window {
                start,
                bytes: answer.bytes,
                attrs: attrs.clone(),
            },
        );
        // The stored window contains `offset` (it is aligned down to it), so the
        // slice hits unless `offset` sits past the fetched bytes (a read past a
        // short window), in which case the server's answer is an empty EOF read.
        inner
            .windows
            .slice(node, offset, len, known_size)
            .unwrap_or(ReadAnswer {
                bytes: Vec::new(),
                eof: offset >= known_size,
                attrs,
            })
    }
}

/// The 2 MiB-aligned window start containing `offset`.
pub(crate) fn window_start(offset: u64) -> u64 {
    offset - (offset % WINDOW_BYTES)
}
