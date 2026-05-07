use omnifs_sdk::prelude::*;

use crate::paper_subtree::PaperSubtree;
use crate::types::PaperKey;
use crate::{Result, State};

pub struct PaperHandlers;

#[handlers]
impl PaperHandlers {
    #[bind("/papers/{paper}")]
    fn paper(_cx: &Cx<State>, paper: PaperKey) -> Result<PaperSubtree> {
        Ok(PaperSubtree { paper })
    }
}
