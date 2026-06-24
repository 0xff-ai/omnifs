//! GitHub object bodies and field projections.

use omnifs_core::ContentType;
use omnifs_sdk::prelude::*;
use omnifs_sdk::repr::{Markdown, Representable};
use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};

use crate::User;

/// The single-item wire body shared by github.issue and github.pull
/// (identical GitHub JSON shape). Not an Object itself; Issue/PullRequest
/// are thin #[serde(transparent)] wrappers.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ItemData {
    pub(crate) number: u64,
    pub(crate) title: String,
    #[serde(default)]
    pub(crate) body: Option<String>,
    pub(crate) state: String,
    #[serde(default)]
    pub(crate) user: Option<User>,
    #[serde(default)]
    pub(crate) updated_at: Option<String>,
    /// Set on issue-list rows that are actually PRs; used by `Issue::load`
    /// to enforce disjointness and by `Repo::issues` to filter rows.
    #[serde(default, skip_serializing)]
    pub(crate) pull_request: Option<IgnoredAny>,
}

impl ItemData {
    pub(crate) fn is_pull_request(&self) -> bool {
        self.pull_request.is_some()
    }

    pub(crate) fn title(&self) -> Result<FileProjection> {
        Ok(FileProjection::text(self.title.clone(), TextFormat::Raw).build())
    }

    pub(crate) fn state(&self) -> Result<FileProjection> {
        Ok(FileProjection::text(self.state.clone(), TextFormat::Raw).build())
    }

    pub(crate) fn user(&self) -> Result<FileProjection> {
        let login = self.user.as_ref().map_or("", |u| u.login.as_str());
        Ok(FileProjection::text(login.to_owned(), TextFormat::Raw).build())
    }

    pub(crate) fn body(&self) -> Result<FileProjection> {
        let body = self.body.as_deref().unwrap_or("");
        Ok(FileProjection::body(body.to_owned())
            .content_type(ContentType::Markdown)
            .build())
    }

    pub(crate) fn markdown(&self) -> Vec<u8> {
        let user = self.user.as_ref().map_or("", |u| u.login.as_str());
        let body = self.body.as_deref().unwrap_or("");
        format!(
            "# {}\n\n- Number: {}\n- State: {}\n- User: {}\n\n{}\n",
            self.title, self.number, self.state, user, body
        )
        .into_bytes()
    }
}

#[omnifs_sdk::object(kind = "github.issue", key = crate::item::IssueKey)]
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct Issue(pub(crate) ItemData);

impl Issue {
    pub(crate) fn title(&self, _key: &crate::item::IssueKey) -> Result<FileProjection> {
        self.0.title()
    }

    pub(crate) fn state(&self, _key: &crate::item::IssueKey) -> Result<FileProjection> {
        self.0.state()
    }

    pub(crate) fn user(&self, _key: &crate::item::IssueKey) -> Result<FileProjection> {
        self.0.user()
    }

    pub(crate) fn body(&self, _key: &crate::item::IssueKey) -> Result<FileProjection> {
        self.0.body()
    }
}

impl Representable<Markdown> for Issue {
    fn represent(&self) -> Vec<u8> {
        self.0.markdown()
    }
}

#[omnifs_sdk::object(kind = "github.pull", key = crate::item::PullKey)]
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct PullRequest(pub(crate) ItemData);

impl PullRequest {
    pub(crate) fn title(&self, _key: &crate::item::PullKey) -> Result<FileProjection> {
        self.0.title()
    }

    pub(crate) fn state(&self, _key: &crate::item::PullKey) -> Result<FileProjection> {
        self.0.state()
    }

    pub(crate) fn user(&self, _key: &crate::item::PullKey) -> Result<FileProjection> {
        self.0.user()
    }

    pub(crate) fn body(&self, _key: &crate::item::PullKey) -> Result<FileProjection> {
        self.0.body()
    }
}

impl Representable<Markdown> for PullRequest {
    fn represent(&self) -> Vec<u8> {
        self.0.markdown()
    }
}

/// A GitHub owner (user or organization) profile. Today the upstream profile
/// JSON is the canonical payload; `profile.md` renders it.
#[omnifs_sdk::object(kind = "github.owner", key = crate::item::OwnerKey)]
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Owner {
    #[serde(default)]
    pub(crate) login: Option<String>,
    #[serde(rename = "type", default)]
    pub(crate) kind: Option<String>,
    #[serde(default)]
    pub(crate) name: Option<String>,
    #[serde(default)]
    pub(crate) bio: Option<String>,
}

impl Representable<Markdown> for Owner {
    fn represent(&self) -> Vec<u8> {
        let login = self.login.as_deref().unwrap_or("");
        let kind = self.kind.as_deref().unwrap_or("User");
        let name = self.name.as_deref().unwrap_or("");
        let bio = self.bio.as_deref().unwrap_or("");
        format!("# {login}\n\n- Type: {kind}\n- Name: {name}\n\n{bio}\n").into_bytes()
    }
}

#[omnifs_sdk::object(kind = "github.repo", key = crate::item::RepoKey)]
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Repo {
    #[serde(default)]
    pub(crate) full_name: Option<String>,
}

/// One issue/PR comment. The list payload carries `id`, so a comment can be
/// stored fresh at listing time and keyed by its own `comment_id`.
#[omnifs_sdk::object(kind = "github.comment", key = crate::item::CommentKey)]
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Comment {
    pub(crate) id: u64,
    pub(crate) user: CommentUser,
    #[serde(default)]
    pub(crate) body: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct CommentUser {
    pub(crate) login: String,
}

impl Comment {
    pub(crate) fn body_md(&self, _key: &crate::item::CommentKey) -> Result<FileProjection> {
        let body = self.body.as_deref().unwrap_or("");
        Ok(FileProjection::body(body.to_owned())
            .content_type(ContentType::Markdown)
            .build())
    }

    pub(crate) fn author(&self, _key: &crate::item::CommentKey) -> Result<FileProjection> {
        Ok(FileProjection::text(self.user.login.clone(), TextFormat::Raw).build())
    }
}

impl Representable<Markdown> for Comment {
    fn represent(&self) -> Vec<u8> {
        let body = self.body.as_deref().unwrap_or("");
        format!("{}:\n{body}\n", self.user.login).into_bytes()
    }
}

#[omnifs_sdk::object(kind = "github.run", key = crate::item::RunKey)]
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct WorkflowRun {
    pub(crate) id: u64,
    pub(crate) status: String,
    #[serde(default)]
    pub(crate) conclusion: Option<String>,
}

impl WorkflowRun {
    pub(crate) fn status(&self, _key: &crate::item::RunKey) -> Result<FileProjection> {
        Ok(FileProjection::text(self.status.clone(), TextFormat::Raw).build())
    }

    pub(crate) fn conclusion(&self, _key: &crate::item::RunKey) -> Result<FileProjection> {
        let c = self.conclusion.clone().unwrap_or_default();
        Ok(FileProjection::text(c, TextFormat::Raw).build())
    }
}
