use omnifs_sdk::prelude::*;

use crate::selector::empty_exhaustive_projection;
use crate::{Result, State};

pub struct RootHandlers;

#[handlers]
impl RootHandlers {
    #[dir("/")]
    fn root(_cx: &DirCx<State>) -> Result<Projection> {
        // Static children (`categories`, `authors`, `search`, `papers`)
        // are auto-derived from the sibling `#[dir(...)]` handlers below.
        Ok(Projection::new())
    }

    // The selector roots are not enumerable: arXiv has no "list all
    // categories / authors / queries" endpoint reachable through the
    // standard search API. (OAI-PMH ListSets exposes the category
    // taxonomy; integrating it is a follow-up.) Users navigate by
    // typing the selector key directly.

    #[dir("/categories")]
    fn categories(_cx: &DirCx<State>) -> Result<Projection> {
        Ok(empty_exhaustive_projection())
    }

    #[dir("/authors")]
    fn authors(_cx: &DirCx<State>) -> Result<Projection> {
        Ok(empty_exhaustive_projection())
    }

    #[dir("/search")]
    fn search(_cx: &DirCx<State>) -> Result<Projection> {
        Ok(empty_exhaustive_projection())
    }

    #[dir("/papers")]
    fn papers(_cx: &DirCx<State>) -> Result<Projection> {
        Ok(empty_exhaustive_projection())
    }
}
