//! Feed operations: one answer of the feed of an object, published part by
//! part, and the projection reads a feed call makes.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::EngineError;
use crate::Runtime;
use crate::cache::MountResources;
use crate::clock;
use crate::effect_apply::EffectApplier;
use crate::inspector;
use crate::object_id::ObjectId;
use crate::runtime::Result;
use omnifs_api::events::InspectorOutcome;
use omnifs_core::path::Path;
use omnifs_wit::host::types as wit_types;
use tracing::Instrument;

use super::namespace::SharedError;

/// The object of a feed: its id, which keys its state in the projection, and
/// the path of its anchor, where the provider runs its feed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FeedObject {
    pub(crate) id: ObjectId,
    pub(crate) path: Path,
}

/// A feed that answers for the path of an operation; `starts` marks an
/// operation on one of its starting paths.
#[derive(Debug, Clone)]
pub struct FeedClaim {
    pub(crate) object: FeedObject,
    pub(crate) starts: bool,
}

impl FeedClaim {
    pub(crate) fn from_wit(claim: &wit_types::FeedClaim) -> anyhow::Result<Self> {
        Ok(Self {
            object: FeedObject {
                id: ObjectId::from_wit(&claim.object.id),
                path: Path::parse(&claim.object.path).map_err(anyhow::Error::from)?,
            },
            starts: claim.starts,
        })
    }
}

/// The outcome of one call of a feed.
#[derive(Debug, Clone, Copy)]
pub(crate) enum FeedOutcome {
    Unchanged,
    Part { more: bool },
}

/// What one answer of a feed changes: the listings it writes into, whose
/// directories keep their identity in filesystems, and the files it writes and
/// the paths it removes, whose answers filesystems drop.
#[derive(Debug, Default)]
pub(crate) struct FeedChanges {
    pub(crate) listings: Vec<Path>,
    pub(crate) paths: Vec<Path>,
}

/// The feed operations in flight, by correlation id, with the anchor path of
/// their object: `projection.listing` answers only for them and only under
/// that path.
#[derive(Default)]
pub(crate) struct FeedOperations {
    paths: dashmap::DashMap<u64, Path>,
}

/// Removes a feed operation from [`FeedOperations`] when its call returns.
struct FeedOperation<'a> {
    operations: &'a FeedOperations,
    id: u64,
}

impl Drop for FeedOperation<'_> {
    fn drop(&mut self) {
        self.operations.paths.remove(&self.id);
    }
}

impl FeedOperations {
    fn begin(&self, id: u64, path: Path) -> FeedOperation<'_> {
        self.paths.insert(id, path);
        FeedOperation {
            operations: self,
            id,
        }
    }
}

/// The reads of the projection a provider makes through the `projection`
/// import.
#[derive(Clone)]
pub(crate) struct ProjectionReads {
    resources: Arc<MountResources>,
    operations: Arc<FeedOperations>,
}

impl ProjectionReads {
    pub(crate) fn new(resources: Arc<MountResources>, operations: Arc<FeedOperations>) -> Self {
        Self {
            resources,
            operations,
        }
    }

    /// The listing of `path` in the projection, for the feed operation `id`
    /// and a path in the subtree of its object; none for a path without one.
    pub(crate) fn listing(
        &self,
        id: u64,
        path: &str,
        form: wit_types::ListingForm,
    ) -> std::result::Result<Option<wit_types::ListingFacts>, wit_types::ProviderError> {
        let Some(object) = self.operations.paths.get(&id).map(|entry| entry.clone()) else {
            return Err(refusal(
                wit_types::ErrorKind::Denied,
                "the projection answers only within a feed operation",
            ));
        };
        let path = Path::parse(path)
            .map_err(|error| refusal(wit_types::ErrorKind::InvalidInput, error.to_string()))?;
        if !path.has_prefix(&object) {
            return Err(refusal(
                wit_types::ErrorKind::Denied,
                format!("{path} is outside the feed of {object}"),
            ));
        }
        let Some(listing) = self
            .resources
            .dirents_payload(&path)
            .map_err(|error| refusal(wit_types::ErrorKind::Internal, error.to_string()))?
        else {
            return Ok(None);
        };
        let names = listing
            .entries()
            .filter(|entry| !crate::tree::synthetic::is_reserved_provider_leaf(&entry.name))
            .map(|entry| entry.name.clone());
        let entries = match form {
            wit_types::ListingForm::Count => {
                wit_types::ListingEntries::Count(u64::try_from(names.count()).unwrap_or(u64::MAX))
            },
            wit_types::ListingForm::Names => wit_types::ListingEntries::Names(names.collect()),
        };
        Ok(Some(wit_types::ListingFacts {
            complete: listing.is_complete_offline(),
            entries,
        }))
    }
}

fn refusal(kind: wit_types::ErrorKind, message: impl Into<String>) -> wit_types::ProviderError {
    wit_types::ProviderError {
        kind,
        message: message.into(),
        retryable: false,
        retry_after: None,
    }
}

impl Runtime {
    /// Run one answer of the feed of `object`, started by the operation at
    /// `start`, in one flight per object id. Returns whether this caller ran
    /// the answer; a caller that waited on another answer of the same feed
    /// gets its result and decides again.
    pub(crate) async fn feed(&self, object: &FeedObject, start: Option<&Path>) -> Result<bool> {
        let led = AtomicBool::new(false);
        let runtime = self;
        self.namespace_flights
            .feed
            .run(
                object.id.clone(),
                || std::future::ready(()),
                || async {
                    led.store(true, Ordering::Relaxed);
                    runtime
                        .run_feed_answer(object, start)
                        .await
                        .map_err(SharedError::from)
                },
            )
            .await
            .map_err(SharedError::into_engine)?;
        Ok(led.load(Ordering::Relaxed))
    }

    /// Call the export until a part without `more`, publishing each part with
    /// its token and inner states as it comes. A failure keeps the token of
    /// the last published part.
    async fn run_feed_answer(&self, object: &FeedObject, start: Option<&Path>) -> Result<()> {
        loop {
            let token = self
                .resources
                .feed_state(object.id.as_bytes())
                .map_err(|error| EngineError::ProviderProtocol(error.to_string()))?
                .map(|state| state.token);
            match self.run_feed_call(object, token, start).await? {
                FeedOutcome::Unchanged | FeedOutcome::Part { more: false } => return Ok(()),
                FeedOutcome::Part { more: true } => {},
            }
        }
    }

    async fn run_feed_call(
        &self,
        object: &FeedObject,
        token: Option<String>,
        start: Option<&Path>,
    ) -> Result<FeedOutcome> {
        let id = self.next_operation_id();
        let span = inspector::provider_span(
            id,
            &self.mount_name,
            &self.provider_name,
            "feed",
            object.path.as_str(),
        );
        let captured_epoch = self.resources.current_epoch();
        let blob_guard = self.resources.blob_publication(id);
        let tokened = token.is_some();
        async {
            let operation = self.feed_operations.begin(id, object.path.clone());
            let (wire_result, effects) = self
                .instance
                .feed(
                    id,
                    object.path.as_str().to_string(),
                    token,
                    start.map(|start| start.as_str().to_string()),
                )
                .await?;
            drop(operation);
            crate::op_validate::validate_feed(&wire_result, &effects, tokened, |tree| {
                self.tree_ref(tree).is_some()
            })
            .map_err(EngineError::ProviderProtocol)?;
            super::lifecycle::inspect_result(&span, &wire_result);
            let pending_blobs = blob_guard.take();
            let answer = self.provider_result(wire_result)?;
            let (outcome, mut transition, changes) = EffectApplier::new(&self.resources)
                .lower_feed(object, answer, &effects, clock::now_millis())
                .map_err(|error| EngineError::ProviderProtocol(error.to_string()))?;
            transition.blobs.extend(pending_blobs);
            self.publish_transition(transition, captured_epoch)?;
            for listing in &changes.listings {
                self.record_dir_changed(listing);
            }
            self.record_view_invalidations(Vec::new(), changes.paths);
            inspector::record_outcome(&span, InspectorOutcome::Ok);
            Ok(outcome)
        }
        .instrument(span.clone())
        .await
    }
}
