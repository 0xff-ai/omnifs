//! Router-side projection helpers.

/// Merge a handler's enumerated entries with the static sibling entries the
/// route table contributes at the same depth: the projection wins name
/// collisions (the handler knows more about a child than the table does),
/// and the result is name-ordered. This merge is why a dir handler only
/// enumerates its dynamic children; literal sibling routes appear in its
/// listing without being re-declared.
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
