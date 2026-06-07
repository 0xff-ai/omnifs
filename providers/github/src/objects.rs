//! GitHub object bodies and field projections.

use omnifs_core::ContentType;
use omnifs_sdk::browse::FileContent;
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
    /// Set on issue-list rows that are actually PRs; used by `IssueKey::load`
    /// to enforce disjointness and by `IssueListKey::list` to filter rows.
    #[serde(default, skip_serializing)]
    pub(crate) pull_request: Option<IgnoredAny>,
}

impl ItemData {
    pub(crate) fn is_pull_request(&self) -> bool {
        self.pull_request.is_some()
    }

    pub(crate) fn title(&self) -> omnifs_sdk::error::Result<FileContent> {
        Ok(FileContent::new(self.title.clone())
            .with_content_type(ContentType::Custom("text/plain")))
    }

    pub(crate) fn state(&self) -> omnifs_sdk::error::Result<FileContent> {
        Ok(FileContent::new(self.state.clone())
            .with_content_type(ContentType::Custom("text/plain")))
    }

    pub(crate) fn user(&self) -> omnifs_sdk::error::Result<FileContent> {
        let login = self.user.as_ref().map_or("", |u| u.login.as_str());
        Ok(FileContent::new(login.to_owned()).with_content_type(ContentType::Custom("text/plain")))
    }

    pub(crate) fn body(&self) -> omnifs_sdk::error::Result<FileContent> {
        let body = self.body.as_deref().unwrap_or("");
        Ok(FileContent::new(body.to_owned()).with_content_type(ContentType::Markdown))
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
    pub(crate) fn title(&self) -> omnifs_sdk::error::Result<FileContent> {
        self.0.title()
    }

    pub(crate) fn state(&self) -> omnifs_sdk::error::Result<FileContent> {
        self.0.state()
    }

    pub(crate) fn user(&self) -> omnifs_sdk::error::Result<FileContent> {
        self.0.user()
    }

    pub(crate) fn body(&self) -> omnifs_sdk::error::Result<FileContent> {
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
    pub(crate) fn title(&self) -> omnifs_sdk::error::Result<FileContent> {
        self.0.title()
    }

    pub(crate) fn state(&self) -> omnifs_sdk::error::Result<FileContent> {
        self.0.state()
    }

    pub(crate) fn user(&self) -> omnifs_sdk::error::Result<FileContent> {
        self.0.user()
    }

    pub(crate) fn body(&self) -> omnifs_sdk::error::Result<FileContent> {
        self.0.body()
    }
}

impl Representable<Markdown> for PullRequest {
    fn represent(&self) -> Vec<u8> {
        self.0.markdown()
    }
}

#[omnifs_sdk::object(kind = "github.repo", key = crate::item::RepoKey)]
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Repo {
    #[serde(default)]
    pub(crate) full_name: Option<String>,
}

#[omnifs_sdk::object(kind = "github.run", key = crate::item::RunKey)]
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Run {
    pub(crate) id: u64,
    pub(crate) status: String,
    #[serde(default)]
    pub(crate) conclusion: Option<String>,
}

impl Run {
    pub(crate) fn status(&self) -> omnifs_sdk::error::Result<FileContent> {
        Ok(FileContent::new(self.status.clone())
            .with_content_type(ContentType::Custom("text/plain")))
    }

    pub(crate) fn conclusion(&self) -> omnifs_sdk::error::Result<FileContent> {
        let c = self.conclusion.clone().unwrap_or_default();
        Ok(FileContent::new(c).with_content_type(ContentType::Custom("text/plain")))
    }
}
