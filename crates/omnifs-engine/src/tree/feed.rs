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
/// the projection takes an answer with its path under the anchor of the
/// innermost claim as `start`; any other operation takes an answer once the
/// feed is past its deadline, outside the rate-limit window of the mount. An
/// operation that waited on another answer of the same feed, or whose feed
/// another answer moved before its flight, decides again after it.
///
/// Returns the path of the fact of the operation in the projection, and the
/// path of the operation under the anchor of the innermost claim. The facts
/// of a fed object live under the anchor of its claim, with its facets at
/// their first choice, for every facet: an operation below an anchor of
/// another facet, or on its listing, has its fact at the same path under the
/// anchor of the claim. The lookup of an anchor has its fact at its own path,
/// in the listing of its parent.
pub(crate) async fn answer_claims(
    runtime: &Runtime,
    path: &Path,
    fact: FedFact,
    claims: &[FeedClaim],
) -> Result<(Path, Path)> {
    // The validation of a fed answer rejects one without claims.
    let anchor = &claims[claims.len() - 1].object.path;
    let under_anchor = &under(anchor, path)?;
    let at = if matches!(fact, FedFact::Lookup) && under_anchor == anchor {
        path
    } else {
        under_anchor
    };
    loop {
        let states = claims
            .iter()
            .map(|claim| runtime.resources.feed_state(claim.object.id.as_bytes()))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| TreeError::internal(error.to_string()))?;
        let answering = states
            .iter()
            .position(|state| state.as_ref().is_some_and(|state| state.ended))
            .unwrap_or(claims.len() - 1);
        let (claim, state) = (&claims[answering], &states[answering]);
        let start = if claim.starts && !has_fact(runtime, at, fact)? {
            Some(Some(under_anchor))
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
            return Ok((at.clone(), under_anchor.clone()));
        };
        if runtime.feed(&claim.object, start, state.as_ref()).await? {
            return Ok((at.clone(), under_anchor.clone()));
        }
    }
}

/// `path`, below an anchor of the object of `anchor` or at it, under
/// `anchor`: the path with the leading segments of its anchor replaced by
/// `anchor`.
fn under(anchor: &Path, path: &Path) -> Result<Path> {
    path.segments()
        .skip(anchor.segments().count())
        .try_fold(anchor.clone(), |at, segment| {
            at.join(segment)
                .map_err(|error| TreeError::internal(error.to_string()))
        })
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

#[cfg(test)]
mod tests {
    use super::under;
    use omnifs_core::path::Path;

    #[test]
    fn a_path_below_an_anchor_of_another_facet_is_under_the_anchor_of_the_claim() {
        let anchor = Path::parse("/octocat/hello/issues/all/3").unwrap();
        for (path, expected) in [
            (
                "/octocat/hello/issues/open/3/comments/7",
                "/octocat/hello/issues/all/3/comments/7",
            ),
            (
                "/octocat/hello/issues/open/3",
                "/octocat/hello/issues/all/3",
            ),
            (
                "/octocat/hello/issues/all/3/title",
                "/octocat/hello/issues/all/3/title",
            ),
        ] {
            assert_eq!(
                under(&anchor, &Path::parse(path).unwrap())
                    .unwrap()
                    .as_str(),
                expected,
                "{path}"
            );
        }
    }
}
