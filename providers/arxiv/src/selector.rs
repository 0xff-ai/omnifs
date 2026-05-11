//! Listing projection helpers shared by all scope modules.

use omnifs_sdk::prelude::*;

use crate::types::{ListedPaper, Listing, ParsedEntry};

impl Listing {
    /// Project a listing of papers under `prefix`. Each paper renders
    /// as a directory; per-paper metadata + version skeleton are
    /// preloaded so subsequent reads avoid round trips.
    pub(crate) fn dir_projection(&self, prefix: &str) -> Projection {
        let mut p = Projection::new();
        p.file_with_content("listing.json", self.summary_json_bytes());
        if self.has_more() {
            p.file_with_content("_more", self.more_marker_bytes());
        }
        for paper in &self.papers {
            p.dir(paper.encoded_key.clone());
            paper.preload_into(&mut p, prefix);
        }
        if self.has_more() {
            p.page(PageStatus::More(Cursor::Opaque(format!(
                "start={}",
                self.papers.len()
            ))));
        } else {
            p.page(PageStatus::Exhaustive);
        }
        p
    }

    fn has_more(&self) -> bool {
        u64::from(self.total_results) > self.papers.len() as u64
    }

    fn more_marker_bytes(&self) -> Vec<u8> {
        format!("fetched {}/{}\n", self.papers.len(), self.total_results).into_bytes()
    }

    fn summary_json_bytes(&self) -> Vec<u8> {
        let payload = serde_json::json!({
            "total_results": self.total_results,
            "listed_results": self.papers.len(),
            "truncated": self.has_more(),
            "request_url": &self.request_url,
        });
        let mut bytes =
            serde_json::to_vec_pretty(&payload).expect("serializing json! is infallible");
        bytes.push(b'\n');
        bytes
    }
}

impl ListedPaper {
    /// Preload everything we already have for this paper from the
    /// listing fetch: the paper dir, `metadata.json`/`links.json` at
    /// the paper root, two binary file entries, and the same skeleton
    /// for each known version. All version-specific data is a pure
    /// local function of the parsed entry — no extra HTTP.
    fn preload_into(&self, p: &mut Projection, prefix: &str) {
        // `prefix` is always a non-empty FS-style path supplied by a
        // scope handler (e.g. `categories/{cat}/{ym}`).
        let paper_base = format!("{}/{}", prefix.trim_matches('/'), self.encoded_key);
        p.preload_dir(paper_base.clone());
        preload_paper_files(p, &paper_base, &self.entry, None);

        let versions_index = format!("{paper_base}/versions");
        p.preload_dir(versions_index.clone());
        for version in 1..=self.entry.latest_version {
            let version_root = format!("{versions_index}/v{version}");
            p.preload_dir(version_root.clone());
            preload_paper_files(p, &version_root, &self.entry, Some(version));
        }
    }
}

fn preload_paper_files(p: &mut Projection, base: &str, entry: &ParsedEntry, version: Option<u32>) {
    p.preload(
        format!("{base}/metadata.json"),
        entry.metadata_json_bytes(version),
    );
    p.preload(
        format!("{base}/links.json"),
        entry.links_json_bytes(version),
    );
    p.preload_entry(
        format!("{base}/paper.pdf"),
        EntryKind::File,
        Some(FileAttrs::deferred(
            Size::Unknown,
            ReadMode::Full,
            Stability::Immutable,
        )),
    );
    p.preload_entry(
        format!("{base}/source.tar.gz"),
        EntryKind::File,
        Some(FileAttrs::deferred(
            Size::Unknown,
            ReadMode::Full,
            Stability::Immutable,
        )),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn more_marker_reports_listed_and_total_results() {
        let listing = Listing {
            request_url: "https://export.arxiv.org/api/query".to_string(),
            total_results: 2001,
            papers: Vec::new(),
        };

        assert!(listing.has_more());
        assert_eq!(
            String::from_utf8(listing.more_marker_bytes()).unwrap(),
            "fetched 0/2001\n"
        );
    }
}
