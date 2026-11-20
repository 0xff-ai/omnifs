//! Feed operations: one answer of the feed of an object, published part by
//! part, or, after the end of the first answer of the feed, kept part by
//! part until its last part publishes them all, and the projection reads a
//! feed call makes.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::EngineError;
use crate::Runtime;
use crate::cache::{FeedState, KeptWrite, MountResources, ProjectionTransition};
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

/// What one call of a feed did: the facts of the feed stand, or a part,
/// published or kept, with the token after it.
enum FeedStep {
    Unchanged,
    Part { more: bool, token: String },
}

/// A part of a later answer of a feed, which the projection keeps until the
/// last part of the answer: the part and the effects of its call.
#[derive(serde::Serialize, serde::Deserialize)]
struct KeptPart {
    part: wit_types::FeedPart,
    effects: wit_types::Effects,
}

impl KeptPart {
    fn encode(&self) -> Result<Vec<u8>> {
        postcard::to_allocvec(self)
            .map_err(|error| EngineError::ProviderProtocol(error.to_string()))
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        postcard::from_bytes(bytes)
            .map_err(|error| EngineError::ProviderProtocol(error.to_string()))
    }
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
            wit_types::ListingForm::Count => wit_types::ListingEntries::Count(
                u64::try_from(names.count())
                    .map_err(|error| refusal(wit_types::ErrorKind::Internal, error.to_string()))?,
            ),
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
    /// `start`, in one flight per object id, while the state of the feed is
    /// still `seen`, the state the caller decided on. Returns whether this
    /// caller ran the answer; a caller that waited on another answer of the
    /// same feed, or whose state another answer moved before its flight, gets
    /// that answer and decides again.
    pub(crate) async fn feed(
        &self,
        object: &FeedObject,
        start: Option<&Path>,
        seen: Option<&FeedState>,
    ) -> Result<bool> {
        let led = AtomicBool::new(false);
        let runtime = self;
        self.namespace_flights
            .feed
            .run(
                object.id.clone(),
                || std::future::ready(()),
                || async {
                    let state = runtime
                        .resources
                        .feed_state(object.id.as_bytes())
                        .map_err(|error| EngineError::ProviderProtocol(error.to_string()))?;
                    if state.as_ref() != seen {
                        return Ok(());
                    }
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

    /// Call the export until a part without `more`. A part of the first
    /// answer of the feed publishes with its token and inner states as it
    /// comes; a part of a later answer is kept, and the last part publishes
    /// the kept parts with it. The answer continues from the token of the
    /// last kept part, else of the last published part, which a failure
    /// keeps.
    async fn run_feed_answer(&self, object: &FeedObject, start: Option<&Path>) -> Result<()> {
        let state = self
            .resources
            .feed_state(object.id.as_bytes())
            .map_err(|error| EngineError::ProviderProtocol(error.to_string()))?;
        let later = state.as_ref().is_some_and(|state| state.ended);
        let kept = self
            .resources
            .kept_parts(object.id.as_bytes())
            .map_err(|error| EngineError::ProviderProtocol(error.to_string()))?;
        let mut token = match kept.last() {
            Some(last) => Some(KeptPart::decode(last)?.part.token),
            None => state.map(|state| state.token),
        };
        let mut kept = !kept.is_empty();
        loop {
            match self
                .run_feed_call(object, token, start, later, kept)
                .await?
            {
                FeedStep::Unchanged | FeedStep::Part { more: false, .. } => return Ok(()),
                FeedStep::Part {
                    more: true,
                    token: next,
                } => {
                    token = Some(next);
                    kept = later;
                },
            }
        }
    }

    /// One call of the feed of `object` from `token`, in a `later` answer,
    /// after the end of the first answer of the feed, with `kept` parts.
    async fn run_feed_call(
        &self,
        object: &FeedObject,
        token: Option<String>,
        start: Option<&Path>,
        later: bool,
        kept: bool,
    ) -> Result<FeedStep> {
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
            let (answer, effects) = match answer {
                wit_types::FeedAnswer::Part(part) if later && part.more => {
                    let token = part.token.clone();
                    self.keep_feed_part(
                        object,
                        &KeptPart { part, effects },
                        pending_blobs,
                        captured_epoch,
                    )?;
                    inspector::record_outcome(&span, InspectorOutcome::Ok);
                    return Ok(FeedStep::Part { more: true, token });
                },
                wit_types::FeedAnswer::Unchanged if kept => {
                    return Err(EngineError::ProviderProtocol(
                        "a feed answered unchanged after the kept parts of its answer".into(),
                    ));
                },
                wit_types::FeedAnswer::Part(part) if kept => {
                    let parts = self
                        .resources
                        .kept_parts(object.id.as_bytes())
                        .map_err(|error| EngineError::ProviderProtocol(error.to_string()))?
                        .iter()
                        .map(|bytes| KeptPart::decode(bytes))
                        .collect::<Result<Vec<_>>>()?;
                    let (part, effects) = compose(parts, KeptPart { part, effects })?;
                    (wit_types::FeedAnswer::Part(part), effects)
                },
                answer => (answer, effects),
            };
            let step = match &answer {
                wit_types::FeedAnswer::Part(part) => FeedStep::Part {
                    more: part.more,
                    token: part.token.clone(),
                },
                wit_types::FeedAnswer::Unchanged => FeedStep::Unchanged,
            };
            let (mut transition, changes) = EffectApplier::new(&self.resources)
                .lower_feed(object, answer, &effects, clock::now_millis())
                .map_err(|error| EngineError::ProviderProtocol(error.to_string()))?;
            transition.blobs.extend(pending_blobs);
            self.publish_transition(transition, captured_epoch)?;
            for listing in &changes.listings {
                let fed = self
                    .resources
                    .is_fed(listing)
                    .map_err(|error| EngineError::ProviderProtocol(error.to_string()))?;
                self.record_dir_changed(listing, fed);
            }
            self.record_view_invalidations(Vec::new(), changes.paths);
            inspector::record_outcome(&span, InspectorOutcome::Ok);
            Ok(step)
        }
        .instrument(span.clone())
        .await
    }

    /// Keep a part of a later answer of the feed of `object`, with the blobs
    /// of its call. Its effects lower first, so that a part that cannot
    /// publish fails at its own call.
    fn keep_feed_part(
        &self,
        object: &FeedObject,
        kept: &KeptPart,
        blobs: Vec<crate::cache::BlobWrite>,
        captured_epoch: u64,
    ) -> Result<()> {
        EffectApplier::new(&self.resources)
            .lower_feed(
                object,
                wit_types::FeedAnswer::Part(kept.part.clone()),
                &kept.effects,
                clock::now_millis(),
            )
            .map_err(|error| EngineError::ProviderProtocol(error.to_string()))?;
        let transition = ProjectionTransition {
            blobs,
            kept: Some(KeptWrite {
                object: object.id.as_bytes().to_vec(),
                part: kept.encode()?,
            }),
            ..ProjectionTransition::default()
        };
        self.publish_transition(transition, captured_epoch)
    }
}

/// The kept parts of a later answer and its last part as one part: the token
/// of the last, the inner states and the facet anchors of each in their
/// order, and the effects of each in their order, where a write of a later
/// part keeps the path that an earlier part removes, or a path above it.
fn compose(
    kept: Vec<KeptPart>,
    last: KeptPart,
) -> Result<(wit_types::FeedPart, wit_types::Effects)> {
    let mut inner = Vec::new();
    let mut anchors = Vec::new();
    let mut effects = wit_types::Effects {
        canonical: Vec::new(),
        fs: Vec::new(),
        invalidations: Vec::new(),
        removals: Vec::new(),
    };
    let token = last.part.token.clone();
    for KeptPart {
        part,
        effects: part_effects,
    } in kept.into_iter().chain([last])
    {
        let written = part_effects
            .fs
            .iter()
            .map(|write| Path::parse(&write.path))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| EngineError::ProviderProtocol(error.to_string()))?;
        let mut removals = Vec::with_capacity(effects.removals.len());
        for removal in effects.removals {
            let removed = Path::parse(&removal)
                .map_err(|error| EngineError::ProviderProtocol(error.to_string()))?;
            if !written.iter().any(|path| path.has_prefix(&removed)) {
                removals.push(removal);
            }
        }
        effects.removals = removals;
        inner.extend(part.inner);
        anchors.extend(part.anchors);
        effects.canonical.extend(part_effects.canonical);
        effects.fs.extend(part_effects.fs);
        effects.invalidations.extend(part_effects.invalidations);
        effects.removals.extend(part_effects.removals);
    }
    Ok((
        wit_types::FeedPart {
            token,
            more: false,
            inner,
            anchors,
        },
        effects,
    ))
}
