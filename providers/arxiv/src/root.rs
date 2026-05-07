use omnifs_sdk::prelude::*;

#[allow(unused_imports)]
use crate::State;

pub struct RootHandlers;

// No handlers needed: `/`, `/categories`, `/authors`, `/search`, and
// `/papers` are all auto-navigable from the route table — each is a
// literal-segment prefix of routes declared in sibling modules. arXiv
// has no "list all categories / authors / queries" endpoint reachable
// through the standard search API, so the selector roots are not
// enumerable; users navigate by typing the selector key directly, and
// the SDK derives non-exhaustive listings since dynamic-capture routes
// live below.

#[handlers]
impl RootHandlers {}
