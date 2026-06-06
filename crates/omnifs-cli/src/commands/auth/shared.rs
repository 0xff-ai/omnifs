//! Shared auth command helpers.

pub(super) fn format_scopes(scopes: &[String]) -> String {
    if scopes.is_empty() {
        "<none>".to_owned()
    } else {
        scopes.join(", ")
    }
}

pub(super) fn format_rfc3339(value: time::OffsetDateTime) -> String {
    value
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| value.to_string())
}
