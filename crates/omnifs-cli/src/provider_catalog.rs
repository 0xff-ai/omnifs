//! Shared provider catalog presentation.
//!
//! The auth label and the column-aligned row shape here are rendered by both
//! `omnifs setup`'s provider catalog and `mount add`'s interactive picker, so
//! a provider looks the same wherever it is listed. Color is deliberately
//! left out: `setup` renders through the flat register's injected
//! `Capabilities`, while `mount add`'s picker draws in raw mode against a
//! live `Stream`, so each caller applies its own color convention on top of
//! the plain text this module returns.

use omnifs_auth::AuthScheme;
use omnifs_provider::ProviderManifest;

use crate::ui::render;

/// Whether a provider needs no sign-in at all: no declared auth scheme, and
/// no interactive config input (a dynamic domain authority or a host-file
/// field) that a headless flow cannot answer on the operator's behalf.
pub(crate) fn needs_no_sign_in(manifest: &ProviderManifest) -> bool {
    manifest.auth.is_none() && !manifest.requires_mount_input()
}

/// The honest auth label for one provider's catalog row, derived only from
/// its manifest: never a hardcoded provider-name table.
pub(crate) fn provider_auth_label(manifest: &ProviderManifest) -> &'static str {
    if needs_no_sign_in(manifest) {
        return "no sign-in";
    }
    if manifest.requires_mount_input() {
        return "needs config";
    }
    match manifest
        .auth
        .as_ref()
        .and_then(|auth| auth.default_scheme())
    {
        Some((_, AuthScheme::StaticToken(_))) => "needs a token",
        _ => "needs sign-in",
    }
}

/// The provider catalog description shown beside its name: the manifest's
/// own description, falling back to its display name.
pub(crate) fn provider_description(manifest: &ProviderManifest) -> &str {
    manifest
        .description
        .as_deref()
        .unwrap_or(&manifest.display_name)
}

/// One provider catalog row's three plain-text facts, before any color.
pub(crate) struct ProviderCatalogRow<'a> {
    pub(crate) name: &'a str,
    pub(crate) description: &'a str,
    pub(crate) label: &'static str,
}

/// Build one row's facts from its manifest.
pub(crate) fn provider_catalog_row(manifest: &ProviderManifest) -> ProviderCatalogRow<'_> {
    ProviderCatalogRow {
        name: &manifest.id,
        description: provider_description(manifest),
        label: provider_auth_label(manifest),
    }
}

/// The fixed gap after the widest name/description column, mirroring
/// `render.rs::LEDGER_GAP`'s role for ledger rows.
const CATALOG_GAP: usize = 4;

/// Column-align a set of catalog rows so every row's label starts at the
/// same column: two gaps sized to the widest name and widest description
/// across the whole set. A key never fires alone: this always sizes against
/// every row in `rows`, the same way `omnifs setup`'s catalog and `mount
/// add`'s picker each pass their whole provider list in one call.
pub(crate) fn align_provider_catalog_rows(rows: &[ProviderCatalogRow<'_>]) -> Vec<String> {
    let name_width = rows
        .iter()
        .map(|row| render::display_width(row.name))
        .max()
        .unwrap_or(0);
    let desc_width = rows
        .iter()
        .map(|row| render::display_width(row.description))
        .max()
        .unwrap_or(0);
    rows.iter()
        .map(|row| {
            let name_pad = name_width.saturating_sub(render::display_width(row.name)) + CATALOG_GAP;
            let desc_pad =
                desc_width.saturating_sub(render::display_width(row.description)) + CATALOG_GAP;
            format!(
                "{}{}{}{}{}",
                row.name,
                " ".repeat(name_pad),
                row.description,
                " ".repeat(desc_pad),
                row.label
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn align_provider_catalog_rows_lines_up_labels_regardless_of_name_or_description_length() {
        let rows = [
            ProviderCatalogRow {
                name: "dns",
                description: "DNS records as files",
                label: "no sign-in",
            },
            ProviderCatalogRow {
                name: "much-longer-name",
                description: "a considerably longer description than the others",
                label: "needs sign-in",
            },
        ];
        let lines = align_provider_catalog_rows(&rows);
        let first_label_column = lines[0].find("no sign-in").unwrap();
        let second_label_column = lines[1].find("needs sign-in").unwrap();
        assert_eq!(first_label_column, second_label_column);
    }
}
