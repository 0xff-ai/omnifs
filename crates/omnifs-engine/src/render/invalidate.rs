use std::hash::Hash;

use crate::tree::InvalidationReport;

use super::identity::{IdentityBody, IdentityKind, IdentityTable};

pub fn stale_ids<Id, Body, Key, Kind, Extra>(
    report: &InvalidationReport,
    table: &IdentityTable<Id, Body, Key, Kind, Extra>,
    mount: &str,
) -> Vec<Id>
where
    Id: Copy + Eq + Hash,
    Body: IdentityBody,
    Key: Clone + Eq + Hash,
    Kind: IdentityKind,
    Extra: Clone,
{
    table
        .entries()
        .iter()
        .filter_map(|entry| {
            let node = entry.value();
            if node.mount_name != mount {
                return None;
            }
            let path = &node.path;
            let exact = report.paths.iter().any(|invalidated| invalidated == path);
            let prefix = report
                .prefixes
                .iter()
                .any(|invalidated| path.has_prefix(invalidated));
            (exact || prefix).then(|| *entry.key())
        })
        .collect()
}
