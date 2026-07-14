//! Linear issue and team object bodies and field projections.

use omnifs_core::ContentType;
use omnifs_sdk::prelude::*;
use omnifs_sdk::repr::{Markdown, Representable};
use serde::Deserialize;

use crate::{IssueKey, TeamPath};

#[omnifs_sdk::object(kind = "linear.issue", key = crate::IssueKey)]
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Issue {
    pub(crate) identifier: String,
    pub(crate) priority: Option<f64>,
    pub(crate) title: String,
    pub(crate) updated_at: Option<String>,
    pub(crate) state: Option<IssueState>,
    pub(crate) assignee: Option<IssueAssignee>,
    pub(crate) description: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct IssueState {
    pub(crate) name: String,
}

#[derive(Clone, Debug, Deserialize)]
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

    /// Linear uses 0 for no priority and 1 through 4 for urgent through low.
    fn priority_label(&self) -> &'static str {
        match self.priority {
            Some(priority) if (0.5..1.5).contains(&priority) => "Urgent",
            Some(priority) if (1.5..2.5).contains(&priority) => "High",
            Some(priority) if (2.5..3.5).contains(&priority) => "Medium",
            Some(priority) if (3.5..4.5).contains(&priority) => "Low",
            _ => "No priority",
        }
    }

    pub(crate) fn title(&self, _key: &IssueKey) -> crate::Result<FileProjection> {
        Ok(FileProjection::text(self.title.as_str(), TextFormat::Newline).build())
    }

    pub(crate) fn state(&self, _key: &IssueKey) -> crate::Result<FileProjection> {
        Ok(FileProjection::text(self.state_label(), TextFormat::Newline).build())
    }

    pub(crate) fn priority(&self, _key: &IssueKey) -> crate::Result<FileProjection> {
        Ok(FileProjection::text(self.priority_label(), TextFormat::Newline).build())
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
            self.priority_label(),
            self.assignee_label(),
            self.description.as_deref().unwrap_or(""),
        )
        .into_bytes()
    }
}

/// A Linear team, projected at `/teams/{team}`. The canonical `item.json` is the
/// raw team node; the anchor also hosts the nested issue collection.
#[omnifs_sdk::object(kind = "linear.team", key = crate::TeamPath)]
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Team {
    pub(crate) key: String,
    pub(crate) name: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) updated_at: Option<String>,
}

impl Team {
    /// Linear has no `If-None-Match`; `updated_at` is the revalidation token.
    pub(crate) fn version(&self) -> Option<&str> {
        self.updated_at.as_deref()
    }

    fn name_label(&self) -> &str {
        self.name.as_deref().unwrap_or("")
    }

    pub(crate) fn name(&self, _key: &TeamPath) -> crate::Result<FileProjection> {
        Ok(FileProjection::text(self.name_label(), TextFormat::Newline).build())
    }

    pub(crate) fn description(&self, _key: &TeamPath) -> crate::Result<FileProjection> {
        let body = self.description.as_deref().unwrap_or("");
        Ok(FileProjection::text(body, TextFormat::Newline)
            .content_type(ContentType::Markdown)
            .build())
    }
}

impl Representable<Markdown> for Team {
    fn represent(&self) -> Vec<u8> {
        format!(
            "# {}\n\n- **Key:** {}\n\n{}\n",
            self.name_label(),
            self.key,
            self.description.as_deref().unwrap_or(""),
        )
        .into_bytes()
    }
}
