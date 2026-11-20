//! `feed` dispatch and the claims of fed faces.

use std::cell::RefCell;
use std::rc::Rc;

use crate::browse::{Effects, FeedClaim};
use crate::cx::Cx;
use crate::error::{ProviderError, Result};
use crate::feed::Feed;
use crate::identity::LogicalId;
use omnifs_core::path::Path;
use omnifs_wit::provider::types as wit_types;

use super::super::compiled::CompiledRouter;
use super::super::object::ClaimSpec;
use super::route_shape::Shape;

impl<S> CompiledRouter<S> {
    /// Run one call of the feed of the object anchored at `path`, from
    /// `token`, for an answer that the operation at `start` started.
    ///
    /// The feed answers [`Feed::unchanged`] or a part with the token after
    /// it, whether more parts follow, and the tokens of the inner feeds it
    /// brought; the facts of the part are the writes of its context. A path
    /// without a feed is not found, so a provider without feeds answers every
    /// call not-found.
    pub async fn feed(
        &self,
        cx: &Cx<S>,
        path: &str,
        token: Option<String>,
        start: Option<String>,
    ) -> Result<(wit_types::FeedAnswer, Effects)> {
        let abs =
            Path::parse(path).map_err(|error| ProviderError::invalid_input(error.to_string()))?;
        let shape = self.shape();
        let route = shape
            .object_route(&abs)
            .ok_or_else(|| ProviderError::not_found(format!("no feed at {path}")))?;
        let feed = route
            .entry
            .feed
            .as_ref()
            .ok_or_else(|| ProviderError::not_found(format!("no feed at {path}")))?;
        let start = start
            .map(|start| {
                let start_abs = Path::parse(&start)
                    .map_err(|error| ProviderError::invalid_input(error.to_string()))?;
                feed.starts
                    .iter()
                    .find(|declared| {
                        start_abs.has_prefix(&abs)
                            && declared.pattern.match_path(&start_abs).is_ok()
                    })
                    .map(|declared| declared.declared)
                    .ok_or_else(|| {
                        ProviderError::invalid_input(format!(
                            "{start} is not a starting path of the feed at {path}"
                        ))
                    })
            })
            .transpose()?;

        let facts = Rc::new(RefCell::new(Effects::new()));
        let answer = super::route_future(
            route.entry.pattern.template(),
            (feed.handler)(
                cx.clone(),
                abs.clone(),
                &route.captures,
                token,
                start,
                Rc::clone(&facts),
            ),
        )
        .await
        .map_err(|error| error.with_context("feed", &abs))?;
        let effects = facts.take();
        let answer = match answer {
            Feed::Unchanged => wit_types::FeedAnswer::Unchanged,
            Feed::Part(part) => {
                let (token, more, inner) = part.into_parts();
                let inner = inner
                    .into_iter()
                    .map(|state| wit_types::FeedState {
                        id: LogicalId::new(state.kind, state.identity).into(),
                        token: state.token,
                    })
                    .collect();
                wit_types::FeedAnswer::Part(wit_types::FeedPart {
                    token,
                    more,
                    inner,
                    anchors: shape.facet_anchors(&effects)?,
                })
            },
        };
        Ok((answer, effects))
    }
}

impl<S> Shape<'_, S> {
    /// Each directory that `effects` write at an anchor of an object of a
    /// feed with a facet at another choice than the first, with the anchor of
    /// its claim.
    fn facet_anchors(&self, effects: &Effects) -> Result<Vec<wit_types::FacetAnchor>> {
        let mut anchors = std::collections::BTreeMap::new();
        for directory in effects.directories() {
            let path = Path::parse(directory)
                .map_err(|error| ProviderError::internal(error.to_string()))?;
            let Some(feed) = self
                .object_route(&path)
                .and_then(|route| route.entry.feed.as_ref())
            else {
                continue;
            };
            let anchor = feed.facets.first_choices(path.as_str())?;
            if anchor != path.as_str() {
                anchors.insert(directory.to_owned(), anchor);
            }
        }
        Ok(anchors
            .into_iter()
            .map(|(path, anchor)| wit_types::FacetAnchor { path, anchor })
            .collect())
    }
}

impl<S> Shape<'_, S> {
    /// The claims of a fed face for an operation on `path`: each claiming
    /// feed whose object the router resolves at the leading segments of
    /// `path`, with its object id and its anchor path, the outer object
    /// first. The anchor path of a claim has the facets of its route at their
    /// first choice: the facts of an object live there for every facet.
    /// `on_face` is false for an operation on a child of a fed dir, which
    /// starts none of its feeds. A path no feed answers for is not found.
    pub(super) fn feed_claims(
        &self,
        specs: &[ClaimSpec],
        path: &Path,
        on_face: bool,
    ) -> Result<Vec<FeedClaim>> {
        let mut claims = Vec::new();
        for spec in specs {
            let entry = &self.router.objects[spec.object];
            let feed = entry.feed.as_ref().ok_or_else(|| {
                ProviderError::internal("a fed face claim names an object without a feed")
            })?;
            let anchor = leading_segments(path, entry.pattern.pattern_len())?;
            let Some(route) = self
                .object_route(&anchor)
                .filter(|route| std::ptr::eq(route.entry, entry))
            else {
                continue;
            };
            let path = feed.facets.first_choices(anchor.as_str())?;
            claims.push(FeedClaim {
                id: (feed.id)(&route.captures)?,
                path: Path::parse(&path)
                    .map_err(|error| ProviderError::internal(error.to_string()))?,
                starts: on_face && spec.starts,
            });
        }
        if claims.is_empty() {
            return Err(ProviderError::not_found(format!(
                "no feed answers for {path}"
            )));
        }
        Ok(claims)
    }
}

/// The path of the first `count` segments of `path`.
fn leading_segments(path: &Path, count: usize) -> Result<Path> {
    let segments = path.segments().take(count).collect::<Vec<_>>();
    Path::parse(&format!("/{}", segments.join("/")))
        .map_err(|error| ProviderError::internal(error.to_string()))
}
