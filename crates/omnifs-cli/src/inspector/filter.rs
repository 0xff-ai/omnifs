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
}
