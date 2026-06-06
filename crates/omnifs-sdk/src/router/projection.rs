//! Router-side projection helpers.

/// Merge projection entry names (resolved through `resolve`) with static
/// sibling entries, projection winning on name collisions, ordered by name.
pub(super) fn merge_entries<'a>(
    names: impl Iterator<Item = &'a str>,
    resolve: impl Fn(&str) -> Option<crate::browse::Entry>,
    static_entries: Vec<crate::browse::Entry>,
) -> Vec<crate::browse::Entry> {
    let mut entries = static_entries
        .into_iter()
        .map(|entry| (entry.name().to_string(), entry))
        .collect::<std::collections::BTreeMap<_, _>>();
    for name in names {
        if let Some(entry) = resolve(name) {
            entries.insert(name.to_string(), entry);
        }
    }
    entries.into_values().collect()
}
