//! Per-issue file handlers for content too large to inline.
//!
//! `description.md` may exceed the per-file inline cap; in that case the
//! listing declares it as a deferred file and the host calls back here
//! to fetch the body. The other per-issue files are always projected
//! inline and never reach this handler.

use omnifs_sdk::prelude::*;

use crate::issues::fetch_issue_by_identifier;
use crate::types::{IssueIdent, StateFilter, TeamKey};
use crate::{Result, State};

pub struct IssueFileHandlers;

#[handlers]
impl IssueFileHandlers {
    #[file("/teams/{team}/issues/{filter}/{ident}/description.md")]
    async fn description(
        cx: &Cx<State>,
        team: TeamKey,
        _filter: StateFilter,
        ident: IssueIdent,
    ) -> Result<FileContent> {
        if ident.team() != &team {
            return Err(ProviderError::not_found("issue identifier mismatch"));
        }
        let issue = fetch_issue_by_identifier(cx, &ident).await?;
        let mut bytes = issue.description.unwrap_or_default().into_bytes();
        if !bytes.ends_with(b"\n") {
            bytes.push(b'\n');
        }
        Ok(FileContent::bytes(bytes))
    }
}
