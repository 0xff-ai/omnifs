//! Linear issue object body and field projections.

use omnifs_core::ContentType;
use omnifs_sdk::browse::FileContent;
use omnifs_sdk::repr::{Markdown, Representable};
use serde::{Deserialize, Serialize};

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

    pub(crate) fn title(&self) -> crate::Result<FileContent> {
        Ok(text_content(newline(&self.title)))
    }

    pub(crate) fn state(&self) -> crate::Result<FileContent> {
        Ok(text_content(newline(self.state_label())))
    }

    pub(crate) fn priority(&self) -> crate::Result<FileContent> {
        Ok(text_content(newline(priority_label(self.priority))))
    }

    pub(crate) fn assignee(&self) -> crate::Result<FileContent> {
        Ok(text_content(newline(self.assignee_label())))
    }

    pub(crate) fn description(&self) -> crate::Result<FileContent> {
        let body = self.description.as_deref().unwrap_or("");
        Ok(FileContent::new(newline(body)).with_content_type(ContentType::Markdown))
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

pub(crate) fn newline(text: &str) -> Vec<u8> {
    let mut bytes = text.as_bytes().to_vec();
    if !bytes.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    bytes
}

fn text_content(bytes: Vec<u8>) -> FileContent {
    FileContent::new(bytes).with_content_type(ContentType::Custom("text/plain"))
}
