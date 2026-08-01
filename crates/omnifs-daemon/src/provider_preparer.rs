//! Bounded daemon-owned provider component preparation.
//!
//! The preparer keeps only provider bytes and phase state. Successful
//! compilation populates Wasmtime's required durable cache and drops the
//! temporary component before the worker completes.

use omnifs_core::{ProviderId, ResourceName};
use omnifs_engine::ComponentEngine;
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};

const COMMAND_CAPACITY: usize = 64;
// Wasmtime compilation is CPU-heavy and cannot be cancelled once it enters
// the blocking pool. One worker still overlaps store startup while bounding
// cold-host shutdown latency and cross-profile contention.
const PROVIDER_WORKERS: usize = 1;

/// Queue order for one provider digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ProviderPriority {
    Embedded,
    Retained,
    Desired,
}

/// Authoritative in-process preparation phase for one digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderPreparationPhase {
    Queued,
    Preparing,
    Retrying,
    Ready,
    Failed,
}

impl ProviderPreparationPhase {
    const fn is_terminal(self) -> bool {
        matches!(self, Self::Ready | Self::Failed)
    }
}

/// Exact bytes and non-secret display identity for one provider digest.
#[derive(Clone)]
pub(crate) struct ProviderPreparationJob {
    provider_id: ProviderId,
    catalog_name: String,
    resource_names: Vec<ResourceName>,
    bytes: Arc<[u8]>,
}

impl ProviderPreparationJob {
    pub(crate) fn new(
        provider_id: ProviderId,
        catalog_name: impl Into<String>,
        mut resource_names: Vec<ResourceName>,
        bytes: Vec<u8>,
    ) -> Result<Self, ProviderPreparerError> {
        let actual = ProviderId::from_wasm_bytes(&bytes);
        if actual != provider_id {
            return Err(ProviderPreparerError::DigestMismatch {
                expected: provider_id,
                actual,
            });
        }
        resource_names.sort();
        resource_names.dedup();
        Ok(Self {
            provider_id,
            catalog_name: catalog_name.into(),
            resource_names,
            bytes: bytes.into(),
        })
    }

    pub(crate) fn total_bytes(&self) -> u64 {
        u64::try_from(self.bytes.len()).unwrap_or(u64::MAX)
    }
}

/// Complete current preparation status for one unique digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderPreparationStatus {
    pub(crate) provider_id: ProviderId,
    pub(crate) catalog_name: String,
    pub(crate) resource_names: Vec<ResourceName>,
    pub(crate) priority: ProviderPriority,
    pub(crate) phase: ProviderPreparationPhase,
    pub(crate) queue_position: Option<u32>,
    pub(crate) queued_digests: u32,
    pub(crate) active_digests: u32,
    pub(crate) completed_digests: u32,
    pub(crate) completed_bytes: u64,
    pub(crate) total_bytes: u64,
    pub(crate) retry_count: u32,
    pub(crate) error_code: Option<String>,
    pub(crate) detail: Option<String>,
}

/// Non-blocking progress callback used by the daemon composition root.
pub(crate) type ProviderProgressSink = dyn Fn(ProviderPreparationStatus) + Send + Sync + 'static;

#[derive(Debug, thiserror::Error)]
pub(crate) enum ProviderPreparerError {
    #[error("provider digest mismatch: expected {expected}, computed {actual}")]
    DigestMismatch {
        expected: ProviderId,
        actual: ProviderId,
    },
    #[error("provider preparer is shutting down")]
    ShuttingDown,
    #[error("provider {0} has not been queued")]
    UnknownProvider(ProviderId),
    #[error("provider {provider_id} preparation failed ({error_code}): {detail}")]
    PreparationFailed {
        provider_id: ProviderId,
        error_code: String,
        detail: String,
    },
    #[error("provider preparer task failed: {0}")]
    Supervisor(String),
}

/// Sole lifecycle owner for the provider preparation actor and its workers.
#[must_use = "ProviderPreparer must be shut down so every worker is joined"]
pub(crate) struct ProviderPreparer {
    handle: ProviderPreparerHandle,
    task: JoinHandle<()>,
}

/// Cloneable request and status handle. This handle never owns worker tasks.
#[derive(Clone)]
pub(crate) struct ProviderPreparerHandle {
    commands: mpsc::Sender<Command>,
    shared: Arc<SharedState>,
}

impl ProviderPreparer {
    pub(crate) fn start(engine: ComponentEngine, sink: Arc<ProviderProgressSink>) -> Self {
        Self::start_with_compiler(
            Arc::new(WasmtimeCompiler { engine }),
            sink,
            PROVIDER_WORKERS,
        )
    }

    fn start_with_compiler(
        compiler: Arc<dyn ProviderCompiler>,
        sink: Arc<ProviderProgressSink>,
        worker_limit: usize,
    ) -> Self {
        assert!(worker_limit > 0, "provider worker limit must be nonzero");
        let (commands, receive) = mpsc::channel(COMMAND_CAPACITY);
        let shared = Arc::new(SharedState::default());
        let actor = Actor {
            compiler,
            sink,
            worker_limit,
            receive,
            shared: Arc::clone(&shared),
            entries: HashMap::new(),
            queue: BinaryHeap::new(),
            workers: JoinSet::new(),
            next_sequence: 0,
            shutting_down: false,
            shutdown_waiter: None,
        };
        let task = tokio::spawn(actor.run());
        Self {
            handle: ProviderPreparerHandle { commands, shared },
            task,
        }
    }

    /// Test-only composition boundary for startup ordering. Production always
    /// constructs this actor from the one required-cache component engine.
    #[cfg(test)]
    pub(crate) fn start_with_test_compiler(
        compiler: Arc<dyn ProviderCompiler>,
        sink: Arc<ProviderProgressSink>,
        worker_limit: usize,
    ) -> Self {
        Self::start_with_compiler(compiler, sink, worker_limit)
    }

    pub(crate) fn handle(&self) -> ProviderPreparerHandle {
        self.handle.clone()
    }

    pub(crate) async fn enqueue(
        &self,
        job: ProviderPreparationJob,
        priority: ProviderPriority,
    ) -> Result<(), ProviderPreparerError> {
        self.handle.enqueue(job, priority).await
    }

    #[cfg(test)]
    pub(crate) fn status(&self, provider_id: ProviderId) -> Option<ProviderPreparationStatus> {
        self.handle.status(provider_id)
    }

    #[cfg(test)]
    pub(crate) async fn wait_ready(
        &self,
        provider_id: ProviderId,
    ) -> Result<(), ProviderPreparerError> {
        self.handle.wait_ready(provider_id).await
    }

    /// Stop admission, cancel queued work, wait for in-flight blocking
    /// compilation, and join the actor plus every worker.
    pub(crate) async fn shutdown(self) -> Result<(), ProviderPreparerError> {
        let Self { handle, task } = self;
        let (finished, wait) = oneshot::channel();
        let requested = handle
            .commands
            .send(Command::Shutdown { finished })
            .await
            .map_err(|_| ProviderPreparerError::ShuttingDown);
        let reply = match requested {
            Ok(()) => wait
                .await
                .map_err(|_| ProviderPreparerError::Supervisor("shutdown reply dropped".into())),
            Err(error) => Err(error),
        };
        task.await
            .map_err(|error| ProviderPreparerError::Supervisor(error.to_string()))?;
        reply
    }
}

impl ProviderPreparerHandle {
    pub(crate) async fn enqueue(
        &self,
        job: ProviderPreparationJob,
        priority: ProviderPriority,
    ) -> Result<(), ProviderPreparerError> {
        self.send_enqueue(job, priority, false).await
    }

    pub(crate) async fn requeue_repaired(
        &self,
        job: ProviderPreparationJob,
        priority: ProviderPriority,
    ) -> Result<(), ProviderPreparerError> {
        self.send_enqueue(job, priority, true).await
    }

    async fn send_enqueue(
        &self,
        job: ProviderPreparationJob,
        priority: ProviderPriority,
        repaired: bool,
    ) -> Result<(), ProviderPreparerError> {
        let (reply, wait) = oneshot::channel();
        self.commands
            .send(Command::Enqueue {
                job,
                priority,
                repaired,
                reply,
            })
            .await
            .map_err(|_| ProviderPreparerError::ShuttingDown)?;
        wait.await
            .map_err(|_| ProviderPreparerError::ShuttingDown)?
    }

    pub(crate) fn status(&self, provider_id: ProviderId) -> Option<ProviderPreparationStatus> {
        let statuses = self
            .shared
            .statuses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let sender = statuses.get(&provider_id)?;
        let mut status = sender.borrow().clone();
        drop(statuses);
        status.queued_digests = self.shared.queued.load(AtomicOrdering::Relaxed);
        status.active_digests = self.shared.active.load(AtomicOrdering::Relaxed);
        status.completed_digests = self.shared.completed.load(AtomicOrdering::Relaxed);
        Some(status)
    }

    pub(crate) async fn wait_ready(
        &self,
        provider_id: ProviderId,
    ) -> Result<(), ProviderPreparerError> {
        let mut status = {
            let statuses = self
                .shared
                .statuses
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            statuses
                .get(&provider_id)
                .ok_or(ProviderPreparerError::UnknownProvider(provider_id))?
                .subscribe()
        };
        loop {
            let current = status.borrow().clone();
            match current.phase {
                ProviderPreparationPhase::Ready => return Ok(()),
                ProviderPreparationPhase::Failed => {
                    return Err(ProviderPreparerError::PreparationFailed {
                        provider_id,
                        error_code: current
                            .error_code
                            .unwrap_or_else(|| "provider_prepare_failed".into()),
                        detail: current
                            .detail
                            .unwrap_or_else(|| "provider preparation failed".into()),
                    });
                },
                ProviderPreparationPhase::Queued
                | ProviderPreparationPhase::Preparing
                | ProviderPreparationPhase::Retrying => {},
            }
            status
                .changed()
                .await
                .map_err(|_| ProviderPreparerError::ShuttingDown)?;
        }
    }
}

#[derive(Default)]
struct SharedState {
    statuses: Mutex<HashMap<ProviderId, watch::Sender<ProviderPreparationStatus>>>,
    queued: AtomicU32,
    active: AtomicU32,
    completed: AtomicU32,
}

enum Command {
    Enqueue {
        job: ProviderPreparationJob,
        priority: ProviderPriority,
        repaired: bool,
        reply: oneshot::Sender<Result<(), ProviderPreparerError>>,
    },
    Shutdown {
        finished: oneshot::Sender<()>,
    },
}

pub(crate) trait ProviderCompiler: Send + Sync + 'static {
    fn prepare(&self, provider_id: ProviderId, bytes: &[u8]) -> Result<(), String>;
}

struct WasmtimeCompiler {
    engine: ComponentEngine,
}

impl ProviderCompiler for WasmtimeCompiler {
    fn prepare(&self, provider_id: ProviderId, bytes: &[u8]) -> Result<(), String> {
        self.engine
            .prepare(provider_id, bytes)
            .map_err(|error| error.to_string())
    }
}

struct Actor {
    compiler: Arc<dyn ProviderCompiler>,
    sink: Arc<ProviderProgressSink>,
    worker_limit: usize,
    receive: mpsc::Receiver<Command>,
    shared: Arc<SharedState>,
    entries: HashMap<ProviderId, Entry>,
    queue: BinaryHeap<QueueItem>,
    workers: JoinSet<Completion>,
    next_sequence: u64,
    shutting_down: bool,
    shutdown_waiter: Option<oneshot::Sender<()>>,
}

struct Entry {
    job: Arc<ProviderPreparationJob>,
    priority: ProviderPriority,
    phase: ProviderPreparationPhase,
    generation: u64,
    sequence: u64,
    retry_count: u32,
    queue_position: Option<u32>,
    error_code: Option<String>,
    detail: Option<String>,
    status: watch::Sender<ProviderPreparationStatus>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct QueueItem {
    provider_id: ProviderId,
    priority: ProviderPriority,
    generation: u64,
    sequence: u64,
}

impl Ord for QueueItem {
    fn cmp(&self, other: &Self) -> Ordering {
        self.priority
            .cmp(&other.priority)
            .then_with(|| other.sequence.cmp(&self.sequence))
    }
}

impl PartialOrd for QueueItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

struct Completion {
    provider_id: ProviderId,
    result: Result<(), String>,
}

impl Actor {
    async fn run(mut self) {
        loop {
            self.dispatch_ready();
            if self.shutting_down && self.workers.is_empty() {
                if let Some(finished) = self.shutdown_waiter.take() {
                    let _ = finished.send(());
                }
                return;
            }

            if self.workers.is_empty() {
                match self.receive.recv().await {
                    Some(command) => self.handle_command(command),
                    None => self.begin_shutdown(None),
                }
                continue;
            }

            tokio::select! {
                command = self.receive.recv(), if !self.shutting_down => {
                    match command {
                        Some(command) => self.handle_command(command),
                        None => self.begin_shutdown(None),
                    }
                }
                completion = self.workers.join_next() => {
                    if let Some(Ok(completion)) = completion {
                        self.finish(completion);
                    }
                }
            }
        }
    }

    fn handle_command(&mut self, command: Command) {
        match command {
            Command::Enqueue {
                job,
                priority,
                repaired,
                reply,
            } => {
                let outcome = if self.shutting_down {
                    Err(ProviderPreparerError::ShuttingDown)
                } else {
                    self.enqueue(job, priority, repaired);
                    Ok(())
                };
                let _ = reply.send(outcome);
            },
            Command::Shutdown { finished } => self.begin_shutdown(Some(finished)),
        }
    }

    fn enqueue(&mut self, job: ProviderPreparationJob, priority: ProviderPriority, repaired: bool) {
        let provider_id = job.provider_id;
        if self.entries.contains_key(&provider_id) {
            self.update_existing(&job, priority, repaired);
            return;
        }

        self.next_sequence = self.next_sequence.wrapping_add(1);
        let sequence = self.next_sequence;
        let queue_position = self.queue_position(priority, sequence);
        let queued = self
            .shared
            .queued
            .fetch_add(1, AtomicOrdering::Relaxed)
            .saturating_add(1);
        let job = Arc::new(job);
        let initial = status_for(
            &job,
            priority,
            ProviderPreparationPhase::Queued,
            Some(queue_position),
            queued,
            self.shared.active.load(AtomicOrdering::Relaxed),
            self.shared.completed.load(AtomicOrdering::Relaxed),
            0,
            None,
            None,
        );
        let (status, _) = watch::channel(initial.clone());
        self.shared
            .statuses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(provider_id, status.clone());
        self.entries.insert(
            provider_id,
            Entry {
                job,
                priority,
                phase: ProviderPreparationPhase::Queued,
                generation: 1,
                sequence,
                retry_count: 0,
                queue_position: Some(queue_position),
                error_code: None,
                detail: None,
                status,
            },
        );
        self.queue.push(QueueItem {
            provider_id,
            priority,
            generation: 1,
            sequence,
        });
        (self.sink)(initial);
    }

    #[allow(clippy::too_many_lines)] // one dedupe, repair, and reprioritization transition
    fn update_existing(
        &mut self,
        job: &ProviderPreparationJob,
        priority: ProviderPriority,
        repaired: bool,
    ) {
        let provider_id = job.provider_id;
        {
            let entry = self
                .entries
                .get_mut(&provider_id)
                .expect("existing provider entry");
            merge_identity(entry, job);
        }

        if repaired
            && self
                .entries
                .get(&provider_id)
                .is_some_and(|entry| entry.phase.is_terminal())
        {
            self.shared.completed.fetch_sub(1, AtomicOrdering::Relaxed);
            {
                let entry = self
                    .entries
                    .get_mut(&provider_id)
                    .expect("existing provider entry");
                entry.priority = entry.priority.max(priority);
                entry.retry_count = entry.retry_count.saturating_add(1);
                entry.phase = ProviderPreparationPhase::Retrying;
                entry.error_code = None;
                entry.detail = None;
            }
            let retrying = self.entry_status(provider_id);
            self.entries
                .get(&provider_id)
                .expect("existing provider entry")
                .status
                .send_replace(retrying.clone());
            (self.sink)(retrying);

            self.next_sequence = self.next_sequence.wrapping_add(1);
            let queued = self
                .shared
                .queued
                .fetch_add(1, AtomicOrdering::Relaxed)
                .saturating_add(1);
            let sequence = self.next_sequence;
            let entry_priority = self
                .entries
                .get(&provider_id)
                .expect("existing provider entry")
                .priority;
            let position = self.queue_position(entry_priority, sequence);
            let (generation, queued_status) = {
                let entry = self
                    .entries
                    .get_mut(&provider_id)
                    .expect("existing provider entry");
                entry.sequence = sequence;
                entry.generation = entry.generation.wrapping_add(1);
                entry.phase = ProviderPreparationPhase::Queued;
                entry.queue_position = Some(position);
                let status = status_for_entry(
                    entry,
                    queued,
                    self.shared.active.load(AtomicOrdering::Relaxed),
                    self.shared.completed.load(AtomicOrdering::Relaxed),
                );
                entry.status.send_replace(status.clone());
                (entry.generation, status)
            };
            (self.sink)(queued_status);
            self.queue.push(QueueItem {
                provider_id,
                priority: entry_priority,
                generation,
                sequence,
            });
            return;
        }

        let should_upgrade = self.entries.get(&provider_id).is_some_and(|entry| {
            entry.phase == ProviderPreparationPhase::Queued && priority > entry.priority
        });
        if should_upgrade {
            let (generation, sequence) = {
                let entry = self
                    .entries
                    .get_mut(&provider_id)
                    .expect("existing provider entry");
                entry.generation = entry.generation.wrapping_add(1);
                entry.priority = priority;
                (entry.generation, entry.sequence)
            };
            self.queue.push(QueueItem {
                provider_id,
                priority,
                generation,
                sequence,
            });
        }
        let status = self.entry_status(provider_id);
        self.entries
            .get(&provider_id)
            .expect("existing provider entry")
            .status
            .send_replace(status.clone());
        (self.sink)(status);
    }

    fn dispatch_ready(&mut self) {
        while !self.shutting_down && self.workers.len() < self.worker_limit {
            let Some(item) = self.pop_current() else {
                return;
            };
            let entry = self
                .entries
                .get_mut(&item.provider_id)
                .expect("queued provider entry");
            entry.phase = ProviderPreparationPhase::Preparing;
            entry.queue_position = None;
            let queued = self
                .shared
                .queued
                .fetch_sub(1, AtomicOrdering::Relaxed)
                .saturating_sub(1);
            let active = self
                .shared
                .active
                .fetch_add(1, AtomicOrdering::Relaxed)
                .saturating_add(1);
            let preparing = status_for_entry(
                entry,
                queued,
                active,
                self.shared.completed.load(AtomicOrdering::Relaxed),
            );
            entry.status.send_replace(preparing.clone());
            (self.sink)(preparing);

            let provider_id = item.provider_id;
            let job = Arc::clone(&entry.job);
            let compiler = Arc::clone(&self.compiler);
            self.workers.spawn(async move {
                let result =
                    tokio::task::spawn_blocking(move || compiler.prepare(provider_id, &job.bytes))
                        .await
                        .map_err(|error| format!("blocking compiler task failed: {error}"))
                        .and_then(std::convert::identity);
                Completion {
                    provider_id,
                    result,
                }
            });
        }
    }

    fn pop_current(&mut self) -> Option<QueueItem> {
        while let Some(item) = self.queue.pop() {
            let Some(entry) = self.entries.get(&item.provider_id) else {
                continue;
            };
            if entry.phase == ProviderPreparationPhase::Queued
                && entry.generation == item.generation
            {
                return Some(item);
            }
        }
        None
    }

    fn finish(&mut self, completion: Completion) {
        let active = self
            .shared
            .active
            .fetch_sub(1, AtomicOrdering::Relaxed)
            .saturating_sub(1);
        let completed = self
            .shared
            .completed
            .fetch_add(1, AtomicOrdering::Relaxed)
            .saturating_add(1);
        let entry = self
            .entries
            .get_mut(&completion.provider_id)
            .expect("active provider entry");
        match completion.result {
            Ok(()) => {
                entry.phase = ProviderPreparationPhase::Ready;
                entry.error_code = None;
                entry.detail = None;
            },
            Err(detail) => {
                entry.phase = ProviderPreparationPhase::Failed;
                entry.error_code = Some("provider_prepare_failed".into());
                entry.detail = Some(detail);
            },
        }
        let status = status_for_entry(
            entry,
            self.shared.queued.load(AtomicOrdering::Relaxed),
            active,
            completed,
        );
        entry.status.send_replace(status.clone());
        (self.sink)(status);
    }

    fn begin_shutdown(&mut self, finished: Option<oneshot::Sender<()>>) {
        if self.shutting_down {
            if let Some(finished) = finished {
                let _ = finished.send(());
            }
            return;
        }
        self.shutting_down = true;
        self.receive.close();
        self.shutdown_waiter = finished;
        self.queue.clear();

        let cancelled: Vec<_> = self
            .entries
            .iter()
            .filter_map(|(provider_id, entry)| {
                (entry.phase == ProviderPreparationPhase::Queued).then_some(*provider_id)
            })
            .collect();
        self.shared.queued.store(0, AtomicOrdering::Relaxed);
        if !cancelled.is_empty() {
            self.shared.completed.fetch_add(
                u32::try_from(cancelled.len()).unwrap_or(u32::MAX),
                AtomicOrdering::Relaxed,
            );
        }
        for provider_id in cancelled {
            let entry = self
                .entries
                .get_mut(&provider_id)
                .expect("queued provider entry");
            entry.phase = ProviderPreparationPhase::Failed;
            entry.queue_position = None;
            entry.error_code = Some("provider_prepare_cancelled".into());
            entry.detail = Some("daemon shutdown cancelled queued preparation".into());
            let status = status_for_entry(
                entry,
                0,
                self.shared.active.load(AtomicOrdering::Relaxed),
                self.shared.completed.load(AtomicOrdering::Relaxed),
            );
            entry.status.send_replace(status.clone());
            (self.sink)(status);
        }
    }

    fn queue_position(&self, priority: ProviderPriority, sequence: u64) -> u32 {
        let ahead = self
            .entries
            .values()
            .filter(|entry| {
                entry.phase == ProviderPreparationPhase::Queued
                    && (entry.priority > priority
                        || entry.priority == priority && entry.sequence < sequence)
            })
            .count();
        u32::try_from(ahead.saturating_add(1)).unwrap_or(u32::MAX)
    }

    fn entry_status(&self, provider_id: ProviderId) -> ProviderPreparationStatus {
        status_for_entry(
            self.entries
                .get(&provider_id)
                .expect("provider status entry"),
            self.shared.queued.load(AtomicOrdering::Relaxed),
            self.shared.active.load(AtomicOrdering::Relaxed),
            self.shared.completed.load(AtomicOrdering::Relaxed),
        )
    }
}

fn merge_identity(entry: &mut Entry, incoming: &ProviderPreparationJob) {
    let mut resource_names = entry.job.resource_names.clone();
    resource_names.extend(incoming.resource_names.iter().cloned());
    resource_names.sort();
    resource_names.dedup();
    let catalog_name = if entry.job.catalog_name.is_empty() {
        incoming.catalog_name.clone()
    } else {
        entry.job.catalog_name.clone()
    };
    entry.job = Arc::new(ProviderPreparationJob {
        provider_id: entry.job.provider_id,
        catalog_name,
        resource_names,
        bytes: Arc::clone(&entry.job.bytes),
    });
}

fn status_for_entry(
    entry: &Entry,
    queued: u32,
    active: u32,
    completed: u32,
) -> ProviderPreparationStatus {
    status_for(
        &entry.job,
        entry.priority,
        entry.phase,
        entry.queue_position,
        queued,
        active,
        completed,
        entry.retry_count,
        entry.error_code.clone(),
        entry.detail.clone(),
    )
}

#[allow(clippy::too_many_arguments)]
fn status_for(
    job: &ProviderPreparationJob,
    priority: ProviderPriority,
    phase: ProviderPreparationPhase,
    queue_position: Option<u32>,
    queued_digests: u32,
    active_digests: u32,
    completed_digests: u32,
    retry_count: u32,
    error_code: Option<String>,
    detail: Option<String>,
) -> ProviderPreparationStatus {
    ProviderPreparationStatus {
        provider_id: job.provider_id,
        catalog_name: job.catalog_name.clone(),
        resource_names: job.resource_names.clone(),
        priority,
        phase,
        queue_position,
        queued_digests,
        active_digests,
        completed_digests,
        completed_bytes: if phase == ProviderPreparationPhase::Ready {
            job.total_bytes()
        } else {
            0
        },
        total_bytes: job.total_bytes(),
        retry_count,
        error_code,
        detail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::Condvar;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::mpsc::UnboundedReceiver;

    struct FakeCompiler {
        state: Mutex<FakeState>,
        gate: Condvar,
        started: mpsc::UnboundedSender<ProviderId>,
        calls: AtomicUsize,
        active: AtomicUsize,
        max_active: AtomicUsize,
    }

    struct FakeState {
        released: bool,
        failures: HashSet<ProviderId>,
    }

    impl FakeCompiler {
        fn new() -> (Arc<Self>, UnboundedReceiver<ProviderId>) {
            let (started, receive) = mpsc::unbounded_channel();
            (
                Arc::new(Self {
                    state: Mutex::new(FakeState {
                        released: false,
                        failures: HashSet::new(),
                    }),
                    gate: Condvar::new(),
                    started,
                    calls: AtomicUsize::new(0),
                    active: AtomicUsize::new(0),
                    max_active: AtomicUsize::new(0),
                }),
                receive,
            )
        }

        fn release(&self) {
            let mut state = self.state.lock().unwrap();
            state.released = true;
            self.gate.notify_all();
        }

        fn block(&self) {
            self.state.lock().unwrap().released = false;
        }

        fn fail(&self, provider_id: ProviderId) {
            self.state.lock().unwrap().failures.insert(provider_id);
        }

        fn repair(&self, provider_id: ProviderId) {
            self.state.lock().unwrap().failures.remove(&provider_id);
        }
    }

    impl ProviderCompiler for FakeCompiler {
        fn prepare(&self, provider_id: ProviderId, _bytes: &[u8]) -> Result<(), String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);
            let _ = self.started.send(provider_id);
            let state = self.state.lock().unwrap();
            let state = self
                .gate
                .wait_while(state, |state| !state.released)
                .unwrap();
            let failed = state.failures.contains(&provider_id);
            drop(state);
            self.active.fetch_sub(1, Ordering::SeqCst);
            if failed {
                Err(format!("synthetic compile failure for {provider_id}"))
            } else {
                Ok(())
            }
        }
    }

    fn job(label: u8, name: &str) -> ProviderPreparationJob {
        let bytes = vec![label; usize::from(label) + 1];
        ProviderPreparationJob::new(
            ProviderId::from_wasm_bytes(&bytes),
            name,
            vec![ResourceName::new(name).unwrap()],
            bytes,
        )
        .unwrap()
    }

    fn recorder() -> (
        Arc<ProviderProgressSink>,
        Arc<Mutex<Vec<ProviderPreparationStatus>>>,
    ) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&events);
        let sink: Arc<ProviderProgressSink> =
            Arc::new(move |status| captured.lock().unwrap().push(status));
        (sink, events)
    }

    fn preparer(
        compiler: Arc<FakeCompiler>,
        sink: Arc<ProviderProgressSink>,
        limit: usize,
    ) -> ProviderPreparer {
        ProviderPreparer::start_with_compiler(compiler, sink, limit)
    }

    #[tokio::test]
    async fn deduplicates_one_digest_and_merges_resource_identity() {
        let (compiler, mut started) = FakeCompiler::new();
        let (sink, _) = recorder();
        let preparer = preparer(Arc::clone(&compiler), sink, 1);
        let blocker = job(1, "blocker");
        let first = job(2, "one");
        let mut duplicate = first.clone();
        duplicate
            .resource_names
            .push(ResourceName::new("alias").unwrap());
        let provider_id = first.provider_id;

        preparer
            .enqueue(blocker.clone(), ProviderPriority::Desired)
            .await
            .unwrap();
        assert_eq!(started.recv().await, Some(blocker.provider_id));
        preparer
            .enqueue(first, ProviderPriority::Embedded)
            .await
            .unwrap();
        preparer
            .enqueue(duplicate, ProviderPriority::Desired)
            .await
            .unwrap();
        compiler.release();
        preparer.wait_ready(provider_id).await.unwrap();
        assert_eq!(started.recv().await, Some(provider_id));
        assert_eq!(compiler.calls.load(Ordering::SeqCst), 2);
        let status = preparer.handle.status(provider_id).unwrap();
        assert_eq!(status.priority, ProviderPriority::Desired);
        assert_eq!(
            status.resource_names,
            [
                ResourceName::new("alias").unwrap(),
                ResourceName::new("one").unwrap()
            ]
        );
        preparer.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn queued_work_uses_desired_then_retained_then_embedded_priority() {
        let (compiler, mut started) = FakeCompiler::new();
        let (sink, events) = recorder();
        let preparer = preparer(Arc::clone(&compiler), sink, 1);
        let active = job(1, "active");
        let embedded = job(2, "embedded");
        let retained = job(3, "retained");
        let desired = job(4, "desired");

        for (job, priority) in [
            (active.clone(), ProviderPriority::Embedded),
            (embedded.clone(), ProviderPriority::Embedded),
            (retained.clone(), ProviderPriority::Retained),
            (desired.clone(), ProviderPriority::Desired),
        ] {
            preparer.enqueue(job, priority).await.unwrap();
        }
        assert_eq!(started.recv().await, Some(active.provider_id));
        compiler.release();
        preparer.wait_ready(desired.provider_id).await.unwrap();
        preparer.wait_ready(retained.provider_id).await.unwrap();
        preparer.wait_ready(embedded.provider_id).await.unwrap();

        let start_order = [
            started.recv().await.unwrap(),
            started.recv().await.unwrap(),
            started.recv().await.unwrap(),
        ];
        assert_eq!(
            start_order,
            [
                desired.provider_id,
                retained.provider_id,
                embedded.provider_id
            ]
        );
        {
            let events = events.lock().unwrap();
            assert!(events.iter().any(|status| {
                status.provider_id == desired.provider_id
                    && status.phase == ProviderPreparationPhase::Preparing
            }));
        }
        preparer.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn never_exceeds_configured_inflight_bound() {
        let (compiler, mut started) = FakeCompiler::new();
        let (sink, _) = recorder();
        let preparer = preparer(Arc::clone(&compiler), sink, 2);
        let jobs = [job(1, "one"), job(2, "two"), job(3, "three")];
        for job in jobs.iter().cloned() {
            preparer
                .enqueue(job, ProviderPriority::Desired)
                .await
                .unwrap();
        }
        started.recv().await.unwrap();
        started.recv().await.unwrap();
        assert!(started.try_recv().is_err());
        assert_eq!(compiler.max_active.load(Ordering::SeqCst), 2);
        compiler.release();
        for job in &jobs {
            preparer.wait_ready(job.provider_id).await.unwrap();
        }
        preparer.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn one_failure_does_not_stop_other_digests() {
        let (compiler, _) = FakeCompiler::new();
        let failed = job(1, "failed");
        let healthy = job(2, "healthy");
        compiler.fail(failed.provider_id);
        compiler.release();
        let (sink, _) = recorder();
        let preparer = preparer(Arc::clone(&compiler), sink, 2);
        preparer
            .enqueue(failed.clone(), ProviderPriority::Desired)
            .await
            .unwrap();
        preparer
            .enqueue(healthy.clone(), ProviderPriority::Desired)
            .await
            .unwrap();

        assert!(matches!(
            preparer.wait_ready(failed.provider_id).await,
            Err(ProviderPreparerError::PreparationFailed { .. })
        ));
        preparer.wait_ready(healthy.provider_id).await.unwrap();
        preparer.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn repaired_terminal_digest_requeues_and_retries() {
        let (compiler, _) = FakeCompiler::new();
        let repaired = job(1, "repaired");
        compiler.fail(repaired.provider_id);
        compiler.release();
        let (sink, events) = recorder();
        let preparer = preparer(Arc::clone(&compiler), sink, 1);
        preparer
            .enqueue(repaired.clone(), ProviderPriority::Retained)
            .await
            .unwrap();
        assert!(preparer.wait_ready(repaired.provider_id).await.is_err());

        compiler.repair(repaired.provider_id);
        preparer
            .handle
            .requeue_repaired(repaired.clone(), ProviderPriority::Desired)
            .await
            .unwrap();
        preparer.wait_ready(repaired.provider_id).await.unwrap();
        let status = preparer.handle.status(repaired.provider_id).unwrap();
        assert_eq!(status.phase, ProviderPreparationPhase::Ready);
        assert_eq!(status.retry_count, 1);
        assert_eq!(compiler.calls.load(Ordering::SeqCst), 2);
        let phases: Vec<_> = events
            .lock()
            .unwrap()
            .iter()
            .filter(|status| status.provider_id == repaired.provider_id)
            .map(|status| status.phase)
            .collect();
        assert!(phases.windows(2).any(|phases| {
            phases
                == [
                    ProviderPreparationPhase::Retrying,
                    ProviderPreparationPhase::Queued,
                ]
        }));
        preparer.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn full_or_dropped_progress_receiver_never_delays_workers() {
        let (compiler, _) = FakeCompiler::new();
        compiler.release();
        let (slow_send, _slow_receive) = mpsc::channel(1);
        let (dropped_send, dropped_receive) = mpsc::channel(1);
        drop(dropped_receive);
        let sink: Arc<ProviderProgressSink> = Arc::new(move |status: ProviderPreparationStatus| {
            let _ = slow_send.try_send(status.clone());
            let _ = dropped_send.try_send(status);
        });
        let preparer = preparer(compiler, sink, 2);
        let jobs = [job(1, "one"), job(2, "two"), job(3, "three")];
        for job in jobs.iter().cloned() {
            preparer
                .enqueue(job, ProviderPriority::Desired)
                .await
                .unwrap();
        }
        for job in &jobs {
            preparer.wait_ready(job.provider_id).await.unwrap();
        }
        preparer.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_cancels_queued_work_and_joins_inflight_compilation() {
        let (compiler, mut started) = FakeCompiler::new();
        compiler.block();
        let (sink, _) = recorder();
        let preparer = preparer(Arc::clone(&compiler), sink, 1);
        let active = job(1, "active");
        let queued = job(2, "queued");
        let handle = preparer.handle();
        preparer
            .enqueue(active.clone(), ProviderPriority::Desired)
            .await
            .unwrap();
        preparer
            .enqueue(queued.clone(), ProviderPriority::Desired)
            .await
            .unwrap();
        assert_eq!(started.recv().await, Some(active.provider_id));

        let shutdown = tokio::spawn(preparer.shutdown());
        tokio::task::yield_now().await;
        assert!(!shutdown.is_finished());
        compiler.release();
        shutdown.await.unwrap().unwrap();

        assert_eq!(compiler.active.load(Ordering::SeqCst), 0);
        let queued_status = handle.status(queued.provider_id).unwrap();
        assert_eq!(queued_status.phase, ProviderPreparationPhase::Failed);
        assert_eq!(
            queued_status.error_code.as_deref(),
            Some("provider_prepare_cancelled")
        );
    }
}
