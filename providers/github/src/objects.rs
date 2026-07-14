//! GitHub object bodies and field projections.

use omnifs_core::ContentType;
use omnifs_sdk::prelude::*;
use omnifs_sdk::repr::{Markdown, Representable};
use serde::Deserialize;
use serde::de::IgnoredAny;

use crate::User;

/// The single-item wire body shared by github.issue and github.pull
/// (identical GitHub JSON shape). Not an Object itself; Issue/PullRequest
/// are thin #[serde(transparent)] wrappers.
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct ItemData {
    pub(crate) number: u64,
    pub(crate) title: String,
    #[serde(default)]
    pub(crate) body: Option<String>,
    pub(crate) state: String,
    #[serde(default)]
    pub(crate) user: Option<User>,
    /// Set on issue-list rows that are actually PRs; used by `Issue::load`
    /// to enforce disjointness and by `Repo::issues` to filter rows.
    #[serde(default)]
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
#[derive(Clone, Debug, Deserialize)]
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
#[derive(Clone, Debug, Deserialize)]
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

#[omnifs_sdk::object(kind = "github.pull_file", key = crate::item::ChangedFileKey)]
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct ChangedFile {
    pub(crate) filename: String,
    pub(crate) status: String,
    #[serde(default)]
    pub(crate) additions: u64,
    #[serde(default)]
    pub(crate) deletions: u64,
    #[serde(default)]
    pub(crate) changes: u64,
    #[serde(default)]
    pub(crate) patch: Option<String>,
    #[serde(default)]
    pub(crate) previous_filename: Option<String>,
}

impl ChangedFile {
    pub(crate) fn filename(&self, _key: &crate::item::ChangedFileKey) -> Result<FileProjection> {
        Ok(FileProjection::text(self.filename.clone(), TextFormat::Raw).build())
    }

    pub(crate) fn status(&self, _key: &crate::item::ChangedFileKey) -> Result<FileProjection> {
        Ok(FileProjection::text(self.status.clone(), TextFormat::Raw).build())
    }

    pub(crate) fn patch(&self, _key: &crate::item::ChangedFileKey) -> Result<FileProjection> {
        let patch = self.patch.as_deref().unwrap_or("");
        Ok(FileProjection::body(patch.to_owned())
            .content_type(ContentType::Custom("text/x-diff"))
            .build())
    }
}

impl Representable<Markdown> for ChangedFile {
    fn represent(&self) -> Vec<u8> {
        let previous = self
            .previous_filename
            .as_ref()
            .map_or(String::new(), |name| {
                format!("- Previous filename: {name}\n")
            });
        format!(
            "# {}\n\n- Status: {}\n- Additions: {}\n- Deletions: {}\n- Changes: {}\n{}",
            self.filename, self.status, self.additions, self.deletions, self.changes, previous
        )
        .into_bytes()
    }
}

#[omnifs_sdk::object(kind = "github.review", key = crate::item::ReviewKey)]
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Review {
    pub(crate) id: u64,
    #[serde(default)]
    pub(crate) user: Option<User>,
    #[serde(default)]
    pub(crate) body: Option<String>,
    #[serde(default)]
    pub(crate) state: Option<String>,
    #[serde(default)]
    pub(crate) submitted_at: Option<String>,
}

impl Review {
    pub(crate) fn state(&self, _key: &crate::item::ReviewKey) -> Result<FileProjection> {
        Ok(FileProjection::text(self.state.clone().unwrap_or_default(), TextFormat::Raw).build())
    }

    pub(crate) fn user(&self, _key: &crate::item::ReviewKey) -> Result<FileProjection> {
        let login = self.user.as_ref().map_or("", |u| u.login.as_str());
        Ok(FileProjection::text(login.to_owned(), TextFormat::Raw).build())
    }

    pub(crate) fn body_md(&self, _key: &crate::item::ReviewKey) -> Result<FileProjection> {
        let body = self.body.as_deref().unwrap_or("");
        Ok(FileProjection::body(body.to_owned())
            .content_type(ContentType::Markdown)
            .build())
    }
}

impl Representable<Markdown> for Review {
    fn represent(&self) -> Vec<u8> {
        let user = self.user.as_ref().map_or("", |u| u.login.as_str());
        let state = self.state.as_deref().unwrap_or("");
        let submitted = self.submitted_at.as_deref().unwrap_or("");
        let body = self.body.as_deref().unwrap_or("");
        format!(
            "# Review {}\n\n- State: {state}\n- User: {user}\n- Submitted: {submitted}\n\n{body}\n",
            self.id
        )
        .into_bytes()
    }
}

#[omnifs_sdk::object(kind = "github.review_comment", key = crate::item::ReviewCommentKey)]
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct ReviewComment {
    pub(crate) id: u64,
    #[serde(default)]
    pub(crate) user: Option<User>,
    #[serde(default)]
    pub(crate) body: Option<String>,
    #[serde(default)]
    pub(crate) path: Option<String>,
    #[serde(default)]
    pub(crate) diff_hunk: Option<String>,
}

impl ReviewComment {
    pub(crate) fn body_md(&self, _key: &crate::item::ReviewCommentKey) -> Result<FileProjection> {
        let body = self.body.as_deref().unwrap_or("");
        Ok(FileProjection::body(body.to_owned())
            .content_type(ContentType::Markdown)
            .build())
    }

    pub(crate) fn author(&self, _key: &crate::item::ReviewCommentKey) -> Result<FileProjection> {
        let login = self.user.as_ref().map_or("", |u| u.login.as_str());
        Ok(FileProjection::text(login.to_owned(), TextFormat::Raw).build())
    }

    pub(crate) fn path(&self, _key: &crate::item::ReviewCommentKey) -> Result<FileProjection> {
        Ok(FileProjection::text(self.path.clone().unwrap_or_default(), TextFormat::Raw).build())
    }
}

impl Representable<Markdown> for ReviewComment {
    fn represent(&self) -> Vec<u8> {
        let user = self.user.as_ref().map_or("", |u| u.login.as_str());
        let path = self.path.as_deref().unwrap_or("");
        let body = self.body.as_deref().unwrap_or("");
        let diff = self.diff_hunk.as_deref().unwrap_or("");
        format!("{user} on `{path}`:\n\n{body}\n\n```diff\n{diff}\n```\n").into_bytes()
    }
}

#[omnifs_sdk::object(kind = "github.check", key = crate::item::CheckRunKey)]
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct CheckRun {
    pub(crate) id: u64,
    pub(crate) name: String,
    pub(crate) status: String,
    #[serde(default)]
    pub(crate) conclusion: Option<String>,
    #[serde(default)]
    pub(crate) html_url: Option<String>,
    #[serde(default)]
    pub(crate) output: Option<CheckOutput>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct CheckOutput {
    #[serde(default)]
    pub(crate) title: Option<String>,
    #[serde(default)]
    pub(crate) summary: Option<String>,
    #[serde(default)]
    pub(crate) text: Option<String>,
}

impl CheckRun {
    pub(crate) fn name(&self, _key: &crate::item::CheckRunKey) -> Result<FileProjection> {
        Ok(FileProjection::text(self.name.clone(), TextFormat::Raw).build())
    }

    pub(crate) fn status(&self, _key: &crate::item::CheckRunKey) -> Result<FileProjection> {
        Ok(FileProjection::text(self.status.clone(), TextFormat::Raw).build())
    }

    pub(crate) fn conclusion(&self, _key: &crate::item::CheckRunKey) -> Result<FileProjection> {
        Ok(
            FileProjection::text(self.conclusion.clone().unwrap_or_default(), TextFormat::Raw)
                .build(),
        )
    }

    pub(crate) fn summary_md(&self, _key: &crate::item::CheckRunKey) -> Result<FileProjection> {
        Ok(FileProjection::body(self.summary_markdown())
            .content_type(ContentType::Markdown)
            .build())
    }

    fn summary_markdown(&self) -> String {
        let Some(output) = &self.output else {
            return String::new();
        };
        let mut summary = String::new();
        if let Some(title) = &output.title {
            summary.push_str("## ");
            summary.push_str(title);
            summary.push_str("\n\n");
        }
        if let Some(body) = &output.summary {
            summary.push_str(body);
            summary.push('\n');
        }
        if let Some(text) = &output.text {
            if !summary.ends_with("\n\n") {
                summary.push('\n');
            }
            summary.push_str(text);
            summary.push('\n');
        }
        summary
    }
}

impl Representable<Markdown> for CheckRun {
    fn represent(&self) -> Vec<u8> {
        let conclusion = self.conclusion.as_deref().unwrap_or("");
        let url = self.html_url.as_deref().unwrap_or("");
        format!(
            "# {}\n\n- Status: {}\n- Conclusion: {conclusion}\n- URL: {url}\n\n{}",
            self.name,
            self.status,
            self.summary_markdown()
        )
        .into_bytes()
    }
}

/// A GitHub owner (user or organization) profile. The upstream profile JSON is
/// the canonical payload; `profile.md` renders it.
#[omnifs_sdk::object(kind = "github.owner", key = crate::item::OwnerKey)]
#[derive(Clone, Debug, Deserialize)]
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
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Repo {}

/// One issue/PR comment. The list payload carries `id`, so a comment can be
/// stored fresh at listing time and keyed by its own `comment_id`.
#[omnifs_sdk::object(kind = "github.comment", key = crate::item::CommentKey)]
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Comment {
    pub(crate) id: u64,
    pub(crate) user: User,
    #[serde(default)]
    pub(crate) body: Option<String>,
}

impl Comment {
    pub(crate) fn body_md(&self, _key: &crate::item::CommentKey) -> Result<FileProjection> {
        let body = self.body.as_deref().unwrap_or("");
        Ok(FileProjection::body(body.to_owned())
            .content_type(ContentType::Markdown)
            .dynamic()
            .build())
    }

    pub(crate) fn author(&self, _key: &crate::item::CommentKey) -> Result<FileProjection> {
        Ok(
            FileProjection::text(self.user.login.clone(), TextFormat::Raw)
                .dynamic()
                .build(),
        )
    }
}

impl Representable<Markdown> for Comment {
    fn represent(&self) -> Vec<u8> {
        let body = self.body.as_deref().unwrap_or("");
        format!("{}:\n{body}\n", self.user.login).into_bytes()
    }
}

#[omnifs_sdk::object(kind = "github.notification", key = crate::item::NotificationKey)]
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Notification {
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) unread: bool,
    #[serde(default)]
    pub(crate) reason: Option<String>,
    #[serde(default)]
    pub(crate) updated_at: Option<String>,
    pub(crate) subject: NotificationSubject,
    #[serde(default)]
    pub(crate) repository: Option<NotificationRepo>,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct NotificationSubject {
    pub(crate) title: String,
    #[serde(rename = "type")]
    pub(crate) kind: String,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct NotificationRepo {
    #[serde(default)]
    pub(crate) full_name: Option<String>,
}

impl Notification {
    pub(crate) fn reason(&self, _key: &crate::item::NotificationKey) -> Result<FileProjection> {
        Ok(FileProjection::text(self.reason.clone().unwrap_or_default(), TextFormat::Raw).build())
    }

    pub(crate) fn subject(&self, _key: &crate::item::NotificationKey) -> Result<FileProjection> {
        Ok(FileProjection::text(self.subject.title.clone(), TextFormat::Raw).build())
    }

    pub(crate) fn item_markdown(&self) -> Vec<u8> {
        let repo = self
            .repository
            .as_ref()
            .and_then(|repo| repo.full_name.as_deref())
            .unwrap_or("");
        let reason = self.reason.as_deref().unwrap_or("");
        let updated = self.updated_at.as_deref().unwrap_or("");
        format!(
            "# {}\n\n- Type: {}\n- Repository: {repo}\n- Reason: {reason}\n- Unread: {}\n- Updated: {updated}\n",
            self.subject.title, self.subject.kind, self.unread
        )
        .into_bytes()
    }
}

impl Representable<Markdown> for Notification {
    fn represent(&self) -> Vec<u8> {
        self.item_markdown()
    }
}

#[omnifs_sdk::object(kind = "github.run", key = crate::item::RunKey)]
#[derive(Clone, Debug, Deserialize)]
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
