use crate::EngineError;
use crate::Runtime;
use crate::cache::{PublicationKey, RecordKind};
use crate::clock::now_millis;
use crate::effect_apply::{EffectApplier, LookupOutcome};
use crate::object_id::ObjectId;
use crate::runtime::Result;
use crate::view::{AttrPayload, CachedCursor, EntryMeta, FileAttrsCache, Stability};
use omnifs_core::path::{Path, Segment};
use omnifs_wit::provider::types as wit_types;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::Arc;
use tokio::sync::Notify;

#[derive(Clone, Debug)]
pub(crate) enum SharedError {
    Provider(wit_types::ProviderError),
    Other(String),
}

impl From<EngineError> for SharedError {
    fn from(error: EngineError) -> Self {
        match error {
            EngineError::ProviderError(error) => Self::Provider(error),
            error => Self::Other(error.to_string()),
        }
    }
}

impl From<anyhow::Error> for SharedError {
    fn from(error: anyhow::Error) -> Self {
        Self::Other(error.to_string())
    }
}

impl SharedError {
    fn into_engine(self) -> EngineError {
        match self {
            Self::Provider(error) => EngineError::ProviderError(error),
            Self::Other(message) => EngineError::ProviderProtocol(message),
        }
    }
}

type Shared<T> = std::result::Result<T, SharedError>;

struct Flight<V> {
    state: Mutex<FlightState<V>>,
    wake: Notify,
}

enum FlightState<V> {
    Running,
    Finished(Shared<V>),
    Cancelled,
}

impl<V> Flight<V> {
    fn new() -> Self {
        Self {
            state: Mutex::new(FlightState::Running),
            wake: Notify::new(),
        }
    }

    async fn wait(&self) -> Option<Shared<V>>
    where
        V: Clone,
    {
        loop {
            let mut notified = Box::pin(self.wake.notified());
            notified.as_mut().enable();
            match &*self.state.lock() {
                FlightState::Running => {},
                FlightState::Finished(result) => return Some(result.clone()),
                FlightState::Cancelled => return None,
            }
            notified.await;
        }
    }
}

struct ExactFlights<K, V> {
    slots: Mutex<HashMap<K, Arc<Flight<V>>>>,
}

impl<K, V> Default for ExactFlights<K, V> {
    fn default() -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
        }
    }
}

enum FlightClaim<'a, K: Eq + Hash, V> {
    Leader(FlightLeader<'a, K, V>),
    Follower(Arc<Flight<V>>),
}

struct FlightLeader<'a, K: Eq + Hash, V> {
    flights: &'a ExactFlights<K, V>,
    key: K,
    flight: Arc<Flight<V>>,
    armed: bool,
}

impl<K, V> ExactFlights<K, V>
where
    K: Eq + Hash + Clone,
{
    fn claim(&self, key: K) -> FlightClaim<'_, K, V> {
        let mut slots = self.slots.lock();
        if let Some(flight) = slots.get(&key) {
            return FlightClaim::Follower(Arc::clone(flight));
        }
        let flight = Arc::new(Flight::new());
        slots.insert(key.clone(), Arc::clone(&flight));
        FlightClaim::Leader(FlightLeader {
            flights: self,
            key,
            flight,
            armed: true,
        })
    }

    async fn run<R, RFut, F, Fut>(&self, key: K, reserve: R, work: F) -> Shared<V>
    where
        R: FnOnce() -> RFut,
        RFut: Future,
        F: FnOnce() -> Fut,
        Fut: Future<Output = Shared<V>>,
        V: Clone,
    {
        let mut reserve = Some(reserve);
        let mut work = Some(work);
        loop {
            match self.claim(key.clone()) {
                FlightClaim::Leader(mut leader) => {
                    let permit = reserve.take().expect("flight reservation is called once")().await;
                    let result = work.take().expect("flight leader work is called once")().await;
                    leader.finish(result.clone());
                    drop(permit);
                    return result;
                },
                FlightClaim::Follower(flight) => {
                    if let Some(result) = flight.wait().await {
                        return result;
                    }
                },
            }
        }
    }
}

impl<K, V> FlightLeader<'_, K, V>
where
    K: Eq + Hash,
{
    fn finish(&mut self, result: Shared<V>) {
        *self.flight.state.lock() = FlightState::Finished(result);
        let mut slots = self.flights.slots.lock();
        if slots
            .get(&self.key)
            .is_some_and(|flight| Arc::ptr_eq(flight, &self.flight))
        {
            slots.remove(&self.key);
        }
        self.armed = false;
        self.flight.wake.notify_waiters();
    }
}

impl<K, V> Drop for FlightLeader<'_, K, V>
where
    K: Eq + Hash,
{
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut slots = self.flights.slots.lock();
        if slots
            .get(&self.key)
            .is_some_and(|flight| Arc::ptr_eq(flight, &self.flight))
        {
            slots.remove(&self.key);
        }
        *self.flight.state.lock() = FlightState::Cancelled;
        self.flight.wake.notify_waiters();
    }
}

#[derive(Clone, Eq, Hash, PartialEq)]
enum ReadKey {
    Path(Path),
    Object(ObjectId),
    Revalidate(ObjectId),
}

pub(crate) struct NamespaceFlights {
    lookup: ExactFlights<Path, LookupOutcome>,
    list: ExactFlights<Path, ListOutcome>,
    read: ExactFlights<ReadKey, ReadOutcome>,
}

impl NamespaceFlights {
    pub(crate) fn new() -> Self {
        Self {
            lookup: ExactFlights::default(),
            list: ExactFlights::default(),
            read: ExactFlights::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    pub meta: EntryMeta,
}

#[derive(Debug, Clone)]
pub struct DirListing {
    pub entries: Vec<DirEntry>,
    pub exhaustive: bool,
    pub validator: Option<String>,
    pub next_cursor: Option<CachedCursor>,
}

#[derive(Debug, Clone)]
pub enum ListOutcome {
    Entries(DirListing),
    Unchanged,
    Subtree(u64),
}

#[derive(Debug, Clone)]
pub enum ReadBytes {
    Inline(Vec<u8>),
    Blob(u64),
    Canonical,
}

#[derive(Debug, Clone)]
pub struct ReadOutcome {
    pub attrs: FileAttrsCache,
    pub bytes: ReadBytes,
    pub content_type: Option<String>,
}

#[derive(Debug, Clone)]
pub struct OpenOutcome {
    pub handle: u64,
    pub attrs: FileAttrsCache,
}

#[derive(Debug, Clone)]
pub struct ChunkOutcome {
    pub content: Vec<u8>,
    pub eof: bool,
}

impl Runtime {
    pub(crate) async fn lookup_child(
        &self,
        parent_path: &Path,
        name: &str,
    ) -> Result<LookupOutcome> {
        let name = Segment::try_from(name)
            .map_err(|error| EngineError::ProviderProtocol(error.to_string()))?;
        let child_path = parent_path.join_segment(&name);
        let now = now_millis();
        if self.resources.negative_for(&child_path, now).is_some() {
            return Ok(LookupOutcome::NotFound);
        }
        let runtime = self;
        let result = self
            .namespace_flights
            .lookup
            .run(
                child_path.clone(),
                || {
                    runtime
                        .resources
                        .reserve(PublicationKey::Path(child_path.clone()))
                },
                || async {
                    let op_gen = runtime.resources.current_generation();
                    let result = runtime
                        .run_lookup_child(parent_path, &name)
                        .await
                        .map_err(SharedError::from)?;
                    EffectApplier::new(&runtime.resources)
                        .lookup(parent_path, &child_path, result, op_gen, now_millis())
                        .map_err(SharedError::from)
                },
            )
            .await
            .map_err(SharedError::into_engine)?;
        Ok(result)
    }

    pub(crate) async fn list_children(
        &self,
        path: &Path,
        cached_validator: Option<String>,
        cursor: Option<CachedCursor>,
    ) -> Result<ListOutcome> {
        let is_continuation = cursor.is_some();
        let runtime = self;
        let result = if is_continuation {
            let _permit = runtime
                .resources
                .reserve(PublicationKey::Path(path.clone()))
                .await;
            let op_gen = runtime.resources.current_generation();
            let result = runtime
                .run_list_children(
                    path,
                    cached_validator,
                    cursor.map(crate::wit_protocol::cached_cursor_to_wit),
                )
                .await?;
            if let wit_types::ListChildrenResult::Entries(ref listing) = result {
                EffectApplier::new(&runtime.resources)
                    .apply_continuation_projection(path, &listing.entries, op_gen)
                    .map_err(|error| {
                        EngineError::ProviderProtocol(format!("cache publication failed: {error}"))
                    })?;
            }
            ListOutcome::from_wit(result)
        } else {
            self.namespace_flights
                .list
                .run(
                    path.clone(),
                    || {
                        runtime
                            .resources
                            .reserve(PublicationKey::Path(path.clone()))
                    },
                    || async {
                        let op_gen = runtime.resources.current_generation();
                        let result = runtime
                            .run_list_children(
                                path,
                                cached_validator,
                                cursor.map(crate::wit_protocol::cached_cursor_to_wit),
                            )
                            .await
                            .map_err(SharedError::from)?;
                        if let wit_types::ListChildrenResult::Entries(ref listing) = result {
                            EffectApplier::new(&runtime.resources)
                                .apply_listing_projection(path, listing, op_gen)
                                .map_err(SharedError::from)?;
                        }
                        Ok(ListOutcome::from_wit(result))
                    },
                )
                .await
                .map_err(SharedError::into_engine)?
        };
        Ok(result)
    }

    pub(crate) async fn read_file(&self, path: &Path, content_type: String) -> Result<ReadOutcome> {
        let now = now_millis();
        let cached_canonical = self.resources.cached_canonical_for(path);
        let mode = if cached_canonical.is_some() && self.resources.view_expired(path, now) {
            ReadMode::Revalidate
        } else {
            ReadMode::Serve
        };
        self.read_file_with_mode(path, content_type, mode, cached_canonical)
            .await
    }

    async fn read_file_with_mode(
        &self,
        path: &Path,
        content_type: String,
        mode: ReadMode,
        cached_canonical: Option<crate::cache::CachedCanonical>,
    ) -> Result<ReadOutcome> {
        let now = now_millis();
        if self.resources.negative_for(path, now).is_some() {
            return Err(enoent(path.as_str()));
        }

        // Single cache lookup: derive both the warm_id (for the exact-flight key and
        // live check) and the CanonicalInput (byte buffer for the provider).
        let (warm_id, cached_canonical) = match cached_canonical {
            Some(crate::cache::CachedCanonical {
                id,
                bytes,
                validator,
            }) => {
                let host_id = ObjectId::from_bytes(id);
                let canonical = host_id.to_wit().map(|id| wit_types::CanonicalInput {
                    id,
                    validator,
                    bytes,
                    revalidate: mode.revalidates(),
                });
                (Some(host_id), canonical)
            },
            None => (None, None),
        };

        let live = warm_id
            .as_ref()
            .and_then(|_| leaf_stability(self, path, now))
            .is_some_and(|s| s == Stability::Live);

        // Warm-but-not-live reads share by object identity, so concurrent user
        // reads of distinct paths that alias the same object share one provider
        // operation. Access-driven revalidation uses a distinct object key
        // because a normal warm read may serve pushed bytes without reloading.
        // Cold reads have no known id yet, so they key on the path.
        let read_key = match &warm_id {
            Some(host_id) => match mode {
                ReadMode::Serve => ReadKey::Object(host_id.clone()),
                ReadMode::Revalidate => ReadKey::Revalidate(host_id.clone()),
            },
            None => ReadKey::Path(path.clone()),
        };
        let publication_key = match &warm_id {
            Some(host_id) => match mode {
                ReadMode::Serve => PublicationKey::Object(host_id.clone()),
                ReadMode::Revalidate => PublicationKey::Revalidate(host_id.clone()),
            },
            None => PublicationKey::Path(path.clone()),
        };
        let runtime = self;
        let reserve = move || runtime.resources.reserve(publication_key);
        let work = move || async move {
            let result = runtime
                .run_read_file(path, content_type, cached_canonical)
                .await
                .map_err(SharedError::from)?;
            match result {
                wit_types::ReadFileOutcome::Found(result) => Ok(ReadOutcome::from_wit(result)),
                wit_types::ReadFileOutcome::NotFound(_) => {
                    Err(SharedError::from(enoent(path.as_str())))
                },
            }
        };
        let result = if live {
            let _permit = reserve().await;
            work().await
        } else {
            self.namespace_flights
                .read
                .run(read_key, reserve, work)
                .await
        }
        .map_err(SharedError::into_engine)?;

        Ok(result)
    }

    pub(crate) async fn open_file(&self, path: &Path) -> Result<OpenOutcome> {
        self.run_open_file(path).await.map(OpenOutcome::from_wit)
    }

    pub(crate) async fn read_chunk(
        &self,
        handle: u64,
        offset: u64,
        length: u32,
    ) -> Result<ChunkOutcome> {
        self.run_read_chunk(handle, offset, length)
            .await
            .map(ChunkOutcome::from_wit)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReadMode {
    Serve,
    Revalidate,
}

impl ReadMode {
    fn revalidates(self) -> bool {
        self == Self::Revalidate
    }
}

impl ListOutcome {
    fn from_wit(result: wit_types::ListChildrenResult) -> Self {
        match result {
            wit_types::ListChildrenResult::Entries(listing) => Self::Entries(DirListing {
                entries: listing
                    .entries
                    .into_iter()
                    .map(DirEntry::from_wit)
                    .collect(),
                exhaustive: listing.exhaustive,
                validator: listing.validator,
                next_cursor: listing
                    .next_cursor
                    .map(crate::wit_protocol::cached_cursor_from_wit),
            }),
            wit_types::ListChildrenResult::Unchanged => Self::Unchanged,
            wit_types::ListChildrenResult::Subtree(tree_ref) => Self::Subtree(tree_ref),
        }
    }
}

impl DirEntry {
    fn from_wit(entry: wit_types::DirEntry) -> Self {
        Self {
            name: entry.name,
            meta: crate::wit_protocol::entry_meta_from_kind(&entry.kind),
        }
    }
}

impl ReadOutcome {
    fn from_wit(result: wit_types::ReadFileResult) -> Self {
        Self {
            attrs: crate::wit_protocol::file_attrs_from_attrs(&result.attrs),
            bytes: ReadBytes::from_wit(result.bytes),
            content_type: result.content_type,
        }
    }
}

impl ReadBytes {
    fn from_wit(bytes: wit_types::ByteSource) -> Self {
        match bytes {
            wit_types::ByteSource::Inline(bytes) => Self::Inline(bytes),
            wit_types::ByteSource::Blob(blob) => Self::Blob(blob),
            wit_types::ByteSource::Canonical => Self::Canonical,
            // The validator rejects a `deferred` read answer before this path is
            // reached; keep a conservative empty inline value if the invariant
            // is ever violated after validation.
            wit_types::ByteSource::Deferred(_) => Self::Inline(Vec::new()),
        }
    }
}

impl OpenOutcome {
    fn from_wit(result: wit_types::OpenFileResult) -> Self {
        Self {
            handle: result.handle,
            attrs: FileAttrsCache::deferred(
                crate::wit_protocol::file_size_from_wit(result.attrs.size),
                crate::view::ReadMode::Ranged,
                crate::wit_protocol::stability_from_wit(result.attrs.stability),
                result.attrs.version_token,
            )
            .expect("provider open attrs are validated before view conversion"),
        }
    }
}

impl ChunkOutcome {
    fn from_wit(result: wit_types::ReadChunkResult) -> Self {
        Self {
            content: result.content,
            eof: result.eof,
        }
    }
}

fn leaf_stability(runtime: &Runtime, path: &Path, now_millis: u64) -> Option<Stability> {
    runtime
        .resources
        .view_get(path, RecordKind::Attr, None, now_millis)
        .and_then(|record| AttrPayload::deserialize(&record.payload))
        .and_then(|attr| attr.meta.attrs().map(FileAttrsCache::stability))
}

fn enoent(path: &str) -> EngineError {
    EngineError::ProviderError(wit_types::ProviderError {
        kind: wit_types::ErrorKind::NotFound,
        message: format!("no such file: {path}"),
        retryable: false,
        retry_after: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn path(value: &str) -> Path {
        Path::parse(value).unwrap()
    }

    #[tokio::test]
    async fn exact_flights_share_the_final_result() {
        let flights = Arc::new(ExactFlights::<Path, ListOutcome>::default());
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let leader = {
            let flights = Arc::clone(&flights);
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            let calls = Arc::clone(&calls);
            tokio::spawn(async move {
                flights
                    .run(
                        path("/x"),
                        || async {},
                        || async move {
                            calls.fetch_add(1, Ordering::SeqCst);
                            started.notify_one();
                            release.notified().await;
                            Ok(ListOutcome::Unchanged)
                        },
                    )
                    .await
            })
        };
        started.notified().await;
        let follower = {
            let flights = Arc::clone(&flights);
            tokio::spawn(async move {
                flights
                    .run(
                        path("/x"),
                        || async {},
                        || async { Ok(ListOutcome::Subtree(9)) },
                    )
                    .await
            })
        };
        release.notify_one();
        assert!(matches!(
            leader.await.unwrap().unwrap(),
            ListOutcome::Unchanged
        ));
        assert!(matches!(
            follower.await.unwrap().unwrap(),
            ListOutcome::Unchanged
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cancelled_exact_flight_releases_its_slot() {
        let flights = Arc::new(ExactFlights::<Path, ListOutcome>::default());
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let leader = {
            let flights = Arc::clone(&flights);
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            tokio::spawn(async move {
                flights
                    .run(
                        path("/x"),
                        || async {},
                        || async move {
                            started.notify_one();
                            release.notified().await;
                            Ok(ListOutcome::Unchanged)
                        },
                    )
                    .await
            })
        };
        started.notified().await;
        let follower = match flights.claim(path("/x")) {
            FlightClaim::Follower(flight) => flight,
            FlightClaim::Leader(_) => panic!("the running leader must own the exact slot"),
        };
        leader.abort();
        let _ = leader.await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), follower.wait())
                .await
                .expect("a late waiter must observe leader cancellation")
                .is_none()
        );
        let recovered = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            flights.run(
                path("/x"),
                || async {},
                || async { Ok(ListOutcome::Subtree(2)) },
            ),
        )
        .await
        .expect("cancellation must release the exact flight")
        .unwrap();
        assert!(matches!(recovered, ListOutcome::Subtree(2)));
    }

    #[test]
    fn shared_provider_error_round_trips_without_losing_retry_fields() {
        let error = EngineError::ProviderError(wit_types::ProviderError {
            kind: wit_types::ErrorKind::RateLimited,
            message: "throttled".to_string(),
            retryable: true,
            retry_after: Some(3),
        });
        let round_tripped = SharedError::from(error).into_engine();
        assert!(round_tripped.is_provider_rate_limited());
        assert!(matches!(
            round_tripped,
            EngineError::ProviderError(error) if error.message == "throttled"
                && error.retryable
                && error.retry_after == Some(3)
        ));
    }
}
