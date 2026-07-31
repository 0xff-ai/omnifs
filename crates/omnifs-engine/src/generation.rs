//! Owned serving-generation transitions and request admission.

use omnifs_auth::{CredentialHealth, CredentialId};
use omnifs_core::{
    CredentialGeneration, CredentialVersion, MountVersion, ProviderRef, ResourceName,
    ResourceRevision,
};
use omnifs_vfs::{
    Namespace, NamespaceEpoch, NamespaceEventHub, NamespaceLease, NamespaceSubscription, NsError,
    ServingNamespace,
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::sync::Notify;
use tokio::time::Instant;

const EVENT_CAPACITY: usize = 1024;

pub struct ServingMountStatus {
    pub name: ResourceName,
    pub provider: ProviderRef,
    pub availability: crate::MountAvailability,
    pub auth_health: Option<CredentialHealth>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GenerationProvenance {
    revision: ResourceRevision,
    mounts: Vec<MountProvenance>,
    credentials: Vec<CredentialProvenance>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountProvenance {
    pub name: ResourceName,
    pub version: MountVersion,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialProvenance {
    pub id: CredentialId,
    pub version: CredentialVersion,
    pub generation: CredentialGeneration,
}

impl GenerationProvenance {
    #[must_use]
    pub fn new(
        revision: ResourceRevision,
        mut mounts: Vec<MountProvenance>,
        mut credentials: Vec<CredentialProvenance>,
    ) -> Self {
        mounts.sort_by(|left, right| left.name.cmp(&right.name));
        credentials.sort_by(|left, right| {
            left.id
                .provider_name()
                .cmp(right.id.provider_name())
                .then_with(|| left.id.scheme().cmp(right.id.scheme()))
                .then_with(|| left.id.account().cmp(right.id.account()))
        });
        Self {
            revision,
            mounts,
            credentials,
        }
    }

    #[must_use]
    pub const fn revision(&self) -> ResourceRevision {
        self.revision
    }

    #[must_use]
    pub fn mount_version(&self, name: &ResourceName) -> Option<MountVersion> {
        self.mounts
            .binary_search_by(|candidate| candidate.name.cmp(name))
            .ok()
            .map(|index| self.mounts[index].version)
    }

    #[must_use]
    pub fn credential_version(
        &self,
        id: &CredentialId,
    ) -> Option<(CredentialVersion, CredentialGeneration)> {
        self.credentials
            .iter()
            .find(|candidate| candidate.id == *id)
            .map(|candidate| (candidate.version, candidate.generation))
    }
}

pub struct PreparedGeneration {
    namespace: Arc<crate::EngineNamespace>,
    table: Arc<crate::MountTable>,
    runtime: Handle,
    provenance: GenerationProvenance,
}

impl PreparedGeneration {
    #[must_use]
    pub fn new(
        table: Arc<crate::MountTable>,
        runtime: Handle,
        provenance: GenerationProvenance,
    ) -> Self {
        let namespace = crate::EngineNamespace::prepared(Arc::clone(&table), runtime.clone());
        Self {
            namespace,
            table,
            runtime,
            provenance,
        }
    }

    #[must_use]
    pub fn provenance(&self) -> &GenerationProvenance {
        &self.provenance
    }

    /// Start generation-owned background work after the durable commit.
    #[must_use]
    pub fn activate(self) -> PublishReadyGeneration {
        PublishReadyGeneration {
            ready: ReadyNamespace::Engine {
                namespace: self.namespace,
                table: self.table,
                runtime: self.runtime,
                provenance: self.provenance,
            },
        }
    }

    #[cfg(test)]
    pub(crate) fn background_active(&self) -> bool {
        self.namespace.background_active()
    }
}

pub struct PublishReadyGeneration {
    ready: ReadyNamespace,
}

enum ReadyNamespace {
    Engine {
        namespace: Arc<crate::EngineNamespace>,
        table: Arc<crate::MountTable>,
        runtime: Handle,
        provenance: GenerationProvenance,
    },
    #[cfg(test)]
    Test {
        namespace: Arc<dyn Namespace>,
        provenance: GenerationProvenance,
    },
}

impl PublishReadyGeneration {
    /// Subscribe to engine events before publication so the pump cannot miss an
    /// event between durable commit and the cell swap.
    #[must_use]
    fn activate(self, epoch: NamespaceEpoch) -> ActiveGeneration {
        let (namespace, table, engine, provenance) = match self.ready {
            ReadyNamespace::Engine {
                namespace,
                table,
                runtime,
                provenance,
            } => {
                table.activate_resources();
                table.activate_timers(&runtime);
                namespace.activate();
                let engine = Arc::clone(&namespace);
                let namespace: Arc<dyn Namespace> = namespace;
                (namespace, Some(table), Some(engine), provenance)
            },
            #[cfg(test)]
            ReadyNamespace::Test {
                namespace,
                provenance,
            } => (namespace, None, None, provenance),
        };
        let events = namespace.subscribe();
        ActiveGeneration {
            inner: GenerationInner {
                epoch,
                namespace,
                table,
                engine,
                provenance,
                admission: Arc::new(Admission::new()),
                cancellation: tokio::sync::watch::channel(false).0,
                events: Mutex::new(Some(events)),
                event_task: Mutex::new(None),
            },
        }
    }
}

struct ActiveGeneration {
    inner: GenerationInner,
}

pub struct RetiredGeneration {
    inner: GenerationInner,
}

impl RetiredGeneration {
    #[must_use]
    pub fn epoch(&self) -> NamespaceEpoch {
        self.inner.epoch
    }

    pub async fn drain(self, grace: Duration) -> DrainOutcome {
        let deadline = Instant::now() + grace;
        loop {
            let active = self.inner.admission.active();
            if active == 0 {
                self.inner.stop_event_task().await;
                self.inner.shutdown_resources(deadline).await;
                return DrainOutcome::Drained;
            }
            let notified = self.inner.admission.drained.notified();
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                self.inner.cancellation.send_replace(true);
                let cancellation_deadline = Instant::now() + grace;
                while self.inner.admission.active() != 0 {
                    let notified = self.inner.admission.drained.notified();
                    if tokio::time::timeout_at(cancellation_deadline, notified)
                        .await
                        .is_err()
                    {
                        let active = self.inner.admission.active();
                        self.inner.stop_event_task().await;
                        return DrainOutcome::Stuck {
                            active,
                            generation: self,
                        };
                    }
                }
                self.inner.stop_event_task().await;
                self.inner.shutdown_resources(cancellation_deadline).await;
                return DrainOutcome::Drained;
            }
        }
    }
}

pub enum DrainOutcome {
    Drained,
    Stuck {
        active: usize,
        generation: RetiredGeneration,
    },
}

impl std::fmt::Debug for DrainOutcome {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Drained => formatter.write_str("Drained"),
            Self::Stuck { active, generation } => formatter
                .debug_struct("Stuck")
                .field("active", active)
                .field("epoch", &generation.epoch())
                .finish(),
        }
    }
}

/// The sole owner of the active serving generation.
pub struct ServingCell {
    active: RwLock<Option<ActiveGeneration>>,
    events: Arc<NamespaceEventHub>,
}

impl ServingCell {
    #[must_use]
    pub fn new(daemon_instance: [u8; 16], initial: PublishReadyGeneration) -> Arc<Self> {
        let initial = initial.activate(NamespaceEpoch::initial(daemon_instance));
        let events = NamespaceEventHub::new(initial.inner.epoch, EVENT_CAPACITY);
        let cell = Arc::new(Self {
            active: RwLock::new(Some(initial)),
            events,
        });
        {
            let active = cell
                .active
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            cell.start_event_task(
                &active
                    .as_ref()
                    .expect("new serving cell has an active generation")
                    .inner,
            );
        }
        cell
    }

    pub fn publish(&self, next: PublishReadyGeneration) -> RetiredGeneration {
        let mut active = self
            .active
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let current = active
            .as_mut()
            .expect("cannot publish after final generation retirement");
        let next_epoch = current
            .inner
            .epoch
            .next()
            .expect("namespace epoch cannot exhaust during one daemon lifetime");
        current.inner.admission.close();
        current.inner.begin_retirement();
        let next = next.activate(next_epoch);
        let retired = active
            .replace(next)
            .expect("serving cell has an active generation");
        self.events
            .advance(next_epoch)
            .expect("ServingCell is the sole NamespaceEventHub publisher");
        self.start_event_task(
            &active
                .as_ref()
                .expect("published generation is active")
                .inner,
        );
        RetiredGeneration {
            inner: retired.inner,
        }
    }

    /// Stop new leases on the current generation before a credential revoke
    /// crosses its next await. Publication follows immediately and owns drain.
    pub fn close_active_admission(&self) {
        self.active
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .expect("cannot close admission after final generation retirement")
            .inner
            .admission
            .close();
    }

    /// Close final request admission during daemon shutdown. The daemon has one
    /// shutdown owner and must call this only after it stops publication.
    pub fn retire_active(&self) -> RetiredGeneration {
        let mut active = self
            .active
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let active = active
            .take()
            .expect("active generation may be retired only once");
        active.inner.admission.close();
        active.inner.begin_retirement();
        RetiredGeneration {
            inner: active.inner,
        }
    }

    #[must_use]
    pub fn mount_statuses(&self) -> Vec<ServingMountStatus> {
        self.mount_statuses_with_provenance().0
    }

    #[must_use]
    pub fn provenance(&self) -> GenerationProvenance {
        self.active
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .expect("no provenance after final generation retirement")
            .inner
            .provenance
            .clone()
    }

    /// One atomic view of mount statuses and the provenance that describes
    /// them. Callers that need both must not call `mount_statuses` and
    /// `provenance` back to back: a `publish` between the two calls would
    /// pair a new generation's mounts with the old generation's provenance,
    /// and `provenance.mount_version` would then return `None` for a mount
    /// that `mount_statuses` just reported as serving.
    ///
    /// Unlike `provenance`, this tolerates a final-retirement `None`
    /// (returning empty statuses and default provenance) because
    /// `mount_statuses` alone always has: a control connection racing
    /// shutdown must see "no mounts", not panic the connection task.
    #[must_use]
    pub fn mount_statuses_with_provenance(
        &self,
    ) -> (Vec<ServingMountStatus>, GenerationProvenance) {
        let active = self
            .active
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(generation) = active.as_ref() else {
            return (Vec::new(), GenerationProvenance::default());
        };
        let statuses = generation
            .inner
            .table
            .as_ref()
            .map_or_else(Vec::new, |table| {
                table
                    .selected_entries()
                    .map(|(name, config, availability, runtime)| ServingMountStatus {
                        name: name.clone(),
                        provider: config.provider.clone(),
                        availability,
                        auth_health: runtime.and_then(|runtime| runtime.auth_health()),
                    })
                    .collect()
            });
        (statuses, generation.inner.provenance.clone())
    }

    fn start_event_task(&self, generation: &GenerationInner) {
        let Some(mut source) = generation
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        else {
            return;
        };
        let epoch = generation.epoch;
        let hub = Arc::clone(&self.events);
        let task = tokio::spawn(async move {
            while let Some(event) = source.recv().await {
                if !hub.publish_if_current(epoch, event) {
                    return;
                }
            }
        });
        *generation
            .event_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(task);
    }
}

impl ServingNamespace for ServingCell {
    fn acquire(&self) -> Result<NamespaceLease, NsError> {
        let active = self
            .active
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let active = active.as_ref().ok_or(NsError::Network)?;
        let guard = active.inner.admission.acquire().ok_or(NsError::Network)?;
        Ok(NamespaceLease::new(
            active.inner.epoch,
            Arc::clone(&active.inner.namespace),
            guard,
            active.inner.cancellation.subscribe(),
        ))
    }

    fn subscribe(&self) -> NamespaceSubscription {
        self.events.subscribe()
    }

    fn current_epoch(&self) -> NamespaceEpoch {
        self.events.current_epoch()
    }
}

struct GenerationInner {
    epoch: NamespaceEpoch,
    namespace: Arc<dyn Namespace>,
    table: Option<Arc<crate::MountTable>>,
    engine: Option<Arc<crate::EngineNamespace>>,
    provenance: GenerationProvenance,
    admission: Arc<Admission>,
    cancellation: tokio::sync::watch::Sender<bool>,
    events: Mutex<Option<omnifs_vfs::EventStream>>,
    event_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl GenerationInner {
    async fn stop_event_task(&self) {
        let task = self
            .event_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
    }

    fn begin_retirement(&self) {
        if let Some(table) = &self.table {
            table.retire_resources();
            table.begin_retirement();
        }
        if let Some(engine) = &self.engine {
            engine.begin_retirement();
        }
    }

    async fn shutdown_resources(&self, deadline: Instant) {
        if let Some(engine) = &self.engine {
            engine.shutdown_background(deadline).await;
        }
        if let Some(table) = &self.table {
            table.shutdown_all_joined(deadline).await;
        }
    }
}

struct Admission {
    open: AtomicBool,
    active: AtomicUsize,
    // The daemon manager owns one joined retired-generation drain, so there is
    // at most one waiter. `notify_one` retains the final-release permit when
    // the waiter has not been polled yet.
    drained: Notify,
}

impl Admission {
    const fn new() -> Self {
        Self {
            open: AtomicBool::new(true),
            active: AtomicUsize::new(0),
            drained: Notify::const_new(),
        }
    }

    fn acquire(self: &Arc<Self>) -> Option<AdmissionGuard> {
        if !self.open.load(Ordering::Acquire) {
            return None;
        }
        self.active.fetch_add(1, Ordering::AcqRel);
        if !self.open.load(Ordering::Acquire) {
            self.release();
            return None;
        }
        Some(AdmissionGuard {
            admission: Arc::clone(self),
        })
    }

    fn close(&self) {
        self.open.store(false, Ordering::Release);
        if self.active() == 0 {
            self.drained.notify_one();
        }
    }

    fn active(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }

    fn release(&self) {
        if self.active.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.drained.notify_one();
        }
    }
}

struct AdmissionGuard {
    admission: Arc<Admission>,
}

impl Drop for AdmissionGuard {
    fn drop(&mut self) {
        self.admission.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future::{BoxFuture, FutureExt as _};
    use omnifs_core::path::Path;
    use omnifs_vfs::{Attrs, DirCursor, DirPage, EventStream, LookupAnswer, ReadAnswer};
    use std::path::PathBuf;
    use tokio::sync::broadcast;

    struct EmptyNamespace {
        events: broadcast::Sender<omnifs_vfs::NsEvent>,
    }

    impl EmptyNamespace {
        fn new() -> Arc<Self> {
            let (events, _) = broadcast::channel(4);
            Arc::new(Self { events })
        }
    }

    impl Namespace for EmptyNamespace {
        fn lookup<'a>(
            &'a self,
            _parent: Path,
            _name: &'a str,
        ) -> BoxFuture<'a, Result<LookupAnswer, NsError>> {
            async { Err(NsError::NotFound) }.boxed()
        }

        fn getattr(&self, _path: Path) -> BoxFuture<'_, Result<Attrs, NsError>> {
            async { Err(NsError::NotFound) }.boxed()
        }

        fn getattr_exact(&self, _path: Path) -> BoxFuture<'_, Result<Attrs, NsError>> {
            async { Err(NsError::NotFound) }.boxed()
        }

        fn readdir(
            &self,
            _path: Path,
            _cursor: DirCursor,
            _budget: usize,
        ) -> BoxFuture<'_, Result<DirPage, NsError>> {
            async { Err(NsError::NotFound) }.boxed()
        }

        fn read(
            &self,
            _path: Path,
            _offset: u64,
            _len: u32,
        ) -> BoxFuture<'_, Result<ReadAnswer, NsError>> {
            async { Err(NsError::NotFound) }.boxed()
        }

        fn readlink(&self, _path: Path) -> BoxFuture<'_, Result<PathBuf, NsError>> {
            async { Err(NsError::NotFound) }.boxed()
        }

        fn subscribe(&self) -> EventStream {
            EventStream::from_broadcast(self.events.subscribe())
        }
    }

    fn ready() -> PublishReadyGeneration {
        ready_with_provenance(GenerationProvenance::default())
    }

    fn ready_with_provenance(provenance: GenerationProvenance) -> PublishReadyGeneration {
        PublishReadyGeneration {
            ready: ReadyNamespace::Test {
                namespace: EmptyNamespace::new(),
                provenance,
            },
        }
    }

    #[tokio::test]
    async fn active_generation_retains_exact_provenance() {
        let mount = ResourceName::new("demo").unwrap();
        let mount_version = MountVersion::from_digest([0x11; 32]);
        let credential = CredentialId::new("demo", "token", "work").unwrap();
        let credential_version = CredentialVersion::initial();
        let credential_generation = CredentialGeneration::initial();
        let provenance = GenerationProvenance::new(
            ResourceRevision::new(7),
            vec![MountProvenance {
                name: mount.clone(),
                version: mount_version,
            }],
            vec![CredentialProvenance {
                id: credential.clone(),
                version: credential_version,
                generation: credential_generation,
            }],
        );
        let cell = ServingCell::new([1; 16], ready_with_provenance(provenance.clone()));

        assert_eq!(cell.provenance(), provenance);
        assert_eq!(cell.provenance().mount_version(&mount), Some(mount_version));
        assert_eq!(
            cell.provenance().credential_version(&credential),
            Some((credential_version, credential_generation))
        );
    }

    #[tokio::test]
    async fn publish_closes_old_admission_and_drain_waits_for_lease() {
        let first = NamespaceEpoch::initial([1; 16]);
        let second = first.next().unwrap();
        let cell = ServingCell::new([1; 16], ready());
        let lease = cell.acquire().unwrap();

        let retired = cell.publish(ready());
        assert_eq!(cell.current_epoch(), second);
        let DrainOutcome::Stuck { active, generation } =
            retired.drain(Duration::from_millis(1)).await
        else {
            panic!("lease should keep retired generation alive");
        };
        assert_eq!(active, 1);
        drop(lease);
        assert!(matches!(
            generation.drain(Duration::from_secs(1)).await,
            DrainOutcome::Drained
        ));
    }

    #[tokio::test]
    async fn retired_generation_drains_after_last_lease() {
        let cell = ServingCell::new([1; 16], ready());
        let lease = cell.acquire().unwrap();
        let retired = cell.publish(ready());
        let drain = tokio::spawn(retired.drain(Duration::from_secs(1)));
        drop(lease);
        assert!(matches!(drain.await.unwrap(), DrainOutcome::Drained));
    }

    #[tokio::test]
    async fn final_retirement_closes_admission_and_drains_existing_lease() {
        let cell = ServingCell::new([1; 16], ready());
        let lease = cell.acquire().unwrap();
        let retired = cell.retire_active();
        assert!(matches!(cell.acquire(), Err(NsError::Network)));
        let drain = tokio::spawn(retired.drain(Duration::from_secs(1)));
        drop(lease);
        assert!(matches!(drain.await.unwrap(), DrainOutcome::Drained));
    }

    #[tokio::test]
    async fn final_release_before_waiter_registration_is_retained() {
        let admission = Arc::new(Admission::new());
        let guard = admission.acquire().expect("admission starts open");
        let notified = admission.drained.notified();

        // Model the drainer's active-count read followed by a delayed poll of
        // the notification future. `notify_one` stores the permit in this gap;
        // `notify_waiters` would lose it because no waiter is registered yet.
        drop(guard);

        tokio::pin!(notified);
        tokio::select! {
            biased;
            () = &mut notified => {}
            () = tokio::task::yield_now() => {
                panic!("final-release notification was lost before waiter registration");
            }
        }
    }

    #[tokio::test]
    async fn drain_cancels_request_after_grace() {
        let cell = ServingCell::new([1; 16], ready());
        let lease = cell.acquire().unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let request = tokio::spawn(async move {
            lease
                .run(async move {
                    let _ = started_tx.send(());
                    std::future::pending::<Result<(), NsError>>().await
                })
                .await
        });
        started_rx.await.unwrap();

        let retired = cell.publish(ready());
        assert!(matches!(
            retired.drain(Duration::from_millis(10)).await,
            DrainOutcome::Drained
        ));
        assert_eq!(request.await.unwrap(), Err(NsError::Network));
    }
}
