//! Runtime actions for paginated directory listings.

use super::Runtime;
use crate::cache::{PublicationKey, RecordKind};
use crate::ops::namespace::ListOutcome;
use crate::view::DirentsPayload;
use omnifs_core::path::Path;

/// Safety cap on `@all` pagination.
pub const MAX_PAGINATION_PAGES: u32 = 20;

/// Outcome of advancing a paged directory by one page.
pub enum NextPageOutcome {
    Loaded { added: usize, more: bool },
    NoMore,
    Error(String),
}

impl Runtime {
    /// Read the accumulated cursor and settle the continuation while holding
    /// the same path publication permit as the initial listing. The lifecycle
    /// owner performs the `AppendPage` transaction before this method observes
    /// the typed outcome.
    pub async fn paginate_next(&self, path: &Path) -> NextPageOutcome {
        let _permit = self
            .resources
            .reserve(PublicationKey::Path(path.clone()))
            .await;
        let captured_epoch = self.resources.current_epoch();
        let record = match self.resources.cache_get(path, RecordKind::Dirents, None) {
            Ok(Some(record)) => record,
            Ok(None) => return NextPageOutcome::NoMore,
            Err(error) => return NextPageOutcome::Error(error.to_string()),
        };
        let dirents: DirentsPayload = match postcard::from_bytes(&record.payload) {
            Ok(dirents) => dirents,
            Err(error) => return NextPageOutcome::Error(error.to_string()),
        };
        let Some(cursor) = dirents.next_cursor else {
            return NextPageOutcome::NoMore;
        };
        match self
            .run_list_children(
                path,
                None,
                Some(crate::wit_protocol::cached_cursor_to_wit(cursor.clone())),
                Some(cursor),
                captured_epoch,
            )
            .await
        {
            Ok(ListOutcome::Entries(listing)) => {
                let more = listing.next_cursor.is_some();
                self.record_dir_changed(path);
                NextPageOutcome::Loaded {
                    added: listing.entries.len(),
                    more,
                }
            },
            Ok(ListOutcome::Unchanged) => {
                self.record_dir_changed(path);
                NextPageOutcome::Loaded {
                    added: 0,
                    more: false,
                }
            },
            Ok(ListOutcome::Subtree(_)) => {
                NextPageOutcome::Error("pagination target resolved to a subtree handoff".into())
            },
            Err(error) => NextPageOutcome::Error(error.to_string()),
        }
    }

    /// Load pages until the provider exhausts the cursor or the safety cap is
    /// reached.
    pub async fn paginate_all(&self, path: &Path) -> String {
        let mut pages = 0;
        let mut added_total = 0;
        loop {
            if pages >= MAX_PAGINATION_PAGES {
                return format!(
                    "loaded {pages} pages (+{added_total}); capped at {MAX_PAGINATION_PAGES} pages\n"
                );
            }
            match self.paginate_next(path).await {
                NextPageOutcome::Loaded { added, more } => {
                    pages += 1;
                    added_total += added;
                    if !more {
                        return format!("loaded {pages} pages (+{added_total}); complete\n");
                    }
                },
                NextPageOutcome::NoMore => {
                    return if pages == 0 {
                        "no more pages\n".into()
                    } else {
                        format!("loaded {pages} pages (+{added_total}); complete\n")
                    };
                },
                NextPageOutcome::Error(error) => {
                    return if pages == 0 {
                        error
                    } else {
                        format!("loaded {pages} pages (+{added_total}); error: {error}")
                    };
                },
            }
        }
    }
}
