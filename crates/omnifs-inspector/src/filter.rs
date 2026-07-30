use super::trace_state::{Operation, OperationStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FilterMode {
    #[default]
    All,
    ErrorsOnly,
}

/// View-time filter and editor state, separated from the App so the App
/// doesn't accumulate a constellation of booleans.
#[derive(Debug, Default)]
pub struct ViewFilter {
    pub mode: FilterMode,
    pub query: String,
    pub editing: bool,
}

impl ViewFilter {
    pub(crate) fn matches(&self, operation: &Operation) -> bool {
        if self.mode == FilterMode::ErrorsOnly && operation.status != OperationStatus::Error {
            return false;
        }
        if self.query.is_empty() {
            return true;
        }
        let needle = self.query.to_ascii_lowercase();
        let haystack = format!(
            "{} {} {} {} {:?}",
            operation.mount,
            operation.path,
            operation.fuse_op,
            operation.provider_name.as_deref().unwrap_or(""),
            operation.outcome
        )
        .to_ascii_lowercase();
        haystack.contains(&needle)
    }

    /// `/`: begin editing the filter query, discarding whatever was typed
    /// on a prior edit.
    pub(crate) fn begin_edit(&mut self) {
        self.editing = true;
        self.query.clear();
    }

    pub(crate) fn push_char(&mut self, ch: char) {
        self.query.push(ch);
    }

    pub(crate) fn backspace(&mut self) {
        self.query.pop();
    }

    /// Esc while editing: discard the in-progress query and stop editing.
    pub(crate) fn cancel_edit(&mut self) {
        self.editing = false;
        self.query.clear();
    }

    /// Enter while editing: keep the query as-is and stop editing.
    pub(crate) fn commit_edit(&mut self) {
        self.editing = false;
    }

    pub(crate) fn toggle_errors_only(&mut self) {
        self.mode = match self.mode {
            FilterMode::ErrorsOnly => FilterMode::All,
            FilterMode::All => FilterMode::ErrorsOnly,
        };
    }
}
