//! Linear issue object body and field projections.

use omnifs_core::ContentType;
use omnifs_sdk::prelude::*;
use omnifs_sdk::repr::{Markdown, Representable};
use serde::{Deserialize, Serialize};

use crate::IssueKey;
use crate::api::priority_label;

#[omnifs_sdk::object(kind = "linear.issue", key = crate::IssueKey)]
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Issue {
    pub(crate) identifier: String,
    pub(crate) number: u64,
    pub(crate) priority: Option<f64>,
    pub(crate) title: String,
    pub(crate) updated_at: Option<String>,
    pub(crate) state: Option<IssueState>,
    pub(crate) assignee: Option<IssueAssignee>,
    pub(crate) description: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct IssueState {
    pub(crate) name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct IssueAssignee {
    pub(crate) display_name: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) email: Option<String>,
}

impl Issue {
    pub(crate) fn version(&self) -> Option<&str> {
        self.updated_at.as_deref()
    }

    fn assignee_label(&self) -> &str {
        self.assignee
            .as_ref()
            .and_then(|a| {
                a.display_name
                    .as_deref()
                    .or(a.name.as_deref())
                    .or(a.email.as_deref())
            })
            .unwrap_or("")
    }

    fn state_label(&self) -> &str {
        self.state.as_ref().map_or("", |s| s.name.as_str())
    }

    pub(crate) fn title(&self, _key: &IssueKey) -> crate::Result<FileProjection> {
        Ok(FileProjection::text(self.title.as_str(), TextFormat::Newline).build())
    }

    pub(crate) fn state(&self, _key: &IssueKey) -> crate::Result<FileProjection> {
        Ok(FileProjection::text(self.state_label(), TextFormat::Newline).build())
    }

    pub(crate) fn priority(&self, _key: &IssueKey) -> crate::Result<FileProjection> {
        Ok(FileProjection::text(priority_label(self.priority), TextFormat::Newline).build())
    }

    pub(crate) fn assignee(&self, _key: &IssueKey) -> crate::Result<FileProjection> {
        Ok(FileProjection::text(self.assignee_label(), TextFormat::Newline).build())
    }

    pub(crate) fn description(&self, _key: &IssueKey) -> crate::Result<FileProjection> {
        let body = self.description.as_deref().unwrap_or("");
        Ok(FileProjection::text(body, TextFormat::Newline)
            .content_type(ContentType::Markdown)
            .build())
    }
}

impl Representable<Markdown> for Issue {
    fn represent(&self) -> Vec<u8> {
        format!(
            "# {} {}\n\n- **State:** {}\n- **Priority:** {}\n- **Assignee:** {}\n\n{}\n",
            self.identifier,
            self.title,
            self.state_label(),
            priority_label(self.priority),
            self.assignee_label(),
            self.description.as_deref().unwrap_or(""),
        )
        .into_bytes()
    }
}
