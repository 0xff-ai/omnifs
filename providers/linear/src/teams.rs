use omnifs_sdk::prelude::*;

use crate::types::TeamKey;
use crate::{Result, State};

pub struct TeamHandlers;

#[handlers]
impl TeamHandlers {
    /// `/teams/{KEY}` exposes a single static `issues` child. The team's
    /// concrete metadata lives under that subtree; the team root itself
    /// is just a navigation node.
    #[dir("/teams/{team}")]
    fn team(_team: TeamKey) -> Result<Projection> {
        let mut projection = Projection::new();
        projection.dir("issues");
        projection.page(PageStatus::Exhaustive);
        Ok(projection)
    }

    /// `/teams/{KEY}/issues` is the parent of the two filter directories.
    #[dir("/teams/{team}/issues")]
    fn issues_root(_team: TeamKey) -> Result<Projection> {
        let mut projection = Projection::new();
        projection.dir("_all");
        projection.dir("_open");
        projection.page(PageStatus::Exhaustive);
        Ok(projection)
    }
}
