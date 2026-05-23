//! CLI presentation policy for human vs machine output.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputFormat {
    Text,
    Json,
}

impl OutputFormat {
    pub(crate) fn from_json_flag(json: bool) -> Self {
        if json { Self::Json } else { Self::Text }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DetailMode {
    Summary,
    Detail,
}

impl DetailMode {
    pub(crate) fn from_flag(detail: bool) -> Self {
        if detail { Self::Detail } else { Self::Summary }
    }

    pub(crate) fn is_detail(self) -> bool {
        matches!(self, Self::Detail)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromptMode {
    Interactive,
    SkipConfirm,
}

impl PromptMode {
    pub(crate) fn from_yes_flag(yes: bool) -> Self {
        if yes {
            Self::SkipConfirm
        } else {
            Self::Interactive
        }
    }

    pub(crate) fn should_skip_confirm(self) -> bool {
        matches!(self, Self::SkipConfirm)
    }
}
