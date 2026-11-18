//! Serving fed paths: the answering feed among the claims of an operation,
//! and when it answers before the operation is served from the projection.

use crate::Runtime;
use crate::cache::RecordKind;
use crate::clock::now_millis;
use crate::ops::feed::FeedClaim;
use crate::view::LookupPayload;
use omnifs_core::path::Path;

use super::error::{Result, TreeError};

/// The fact an operation on a fed path serves.
#[derive(Debug, Clone, Copy)]
pub(crate) enum FedFact {
    Lookup,
    Listing,
    File,
}

/// Have the answering feed among `claims` answer when the operation on
/// `path` needs it: the outer claim whose first answer ended, else the
/// innermost one. An operation that starts that feed without its fact in
/// the projection takes an answer with its path as `start`; any other
/// operation takes an answer once the feed is past its deadline, outside the
/// rate-limit window of the mount. An operation that waited on another
/// answer of the same feed decides again after it.
pub(crate) async fn answer_claims(
    runtime: &Runtime,
    path: &Path,
    fact: FedFact,
    claims: &[FeedClaim],
) -> Result<()> {
    let innermost = claims
        .len()
        .checked_sub(1)
        .ok_or_else(|| TreeError::internal("a fed answer carries no claims"))?;
    loop {
        let states = claims
            .iter()
            .map(|claim| runtime.resources.feed_state(claim.object.id.as_bytes()))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| TreeError::internal(error.to_string()))?;
        let answering = states
            .iter()
            .position(|state| state.as_ref().is_some_and(|state| state.ended))
            .unwrap_or(innermost);
        let (claim, state) = (&claims[answering], &states[answering]);
        let start = if claim.starts && !has_fact(runtime, path, fact)? {
            Some(Some(path))
        } else if runtime.rate_limited_until().is_none()
            && state
                .as_ref()
                .is_none_or(|state| state.deadline <= now_millis())
        {
            Some(None)
        } else {
            None
        };
        let Some(start) = start else {
            return Ok(());
        };
        if runtime.feed(&claim.object, start).await? {
            return Ok(());
        }
    }
}

/// Whether the projection has the fact an operation on `path` serves: its
/// positive lookup, its file, or its complete listing.
fn has_fact(runtime: &Runtime, path: &Path, fact: FedFact) -> Result<bool> {
    let resources = &runtime.resources;
    let found = match fact {
        FedFact::Lookup => resources
            .lookup_payload(path)
            .map(|lookup| matches!(lookup, Some(LookupPayload::Positive(_)))),
        FedFact::Listing => resources
            .dirents_payload(path)
            .map(|listing| listing.is_some_and(|listing| listing.is_complete_offline())),
        FedFact::File => resources
            .cache_get(path, RecordKind::File, None)
            .map(|record| record.is_some()),
    };
    found.map_err(|error| TreeError::internal(error.to_string()))
}
