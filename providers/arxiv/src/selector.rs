//! Listing projection helpers shared by all scope modules.

use omnifs_sdk::prelude::*;

use crate::query::MAX_WINDOW_INDEX;
use crate::types::{ListedPaper, Listing, ParsedEntry};

/// Project the `0..=14` window dirs under a `new/` or `updated/` index.
pub(crate) fn window_index_projection() -> Projection {
    let mut p = Projection::new();
    for i in 0..=MAX_WINDOW_INDEX {
        p.dir(format!("{i}"));
    }
    p.page(PageStatus::Exhaustive);
    p
}

impl Listing {
    /// Project a listing of papers under `prefix`. Each paper renders
    /// as a directory; per-paper metadata + version skeleton are
    /// projected so subsequent reads avoid round trips.
    pub(crate) fn dir_projection(&self, prefix: &str) -> Projection {
        let mut p = Projection::new();
        p.file_with_content("listing.json", self.summary_json_bytes());
        for paper in &self.papers {
            p.dir(paper.encoded_key.clone());
            paper.project_into(&mut p, prefix);
        }
        if u64::from(self.total_results) > self.papers.len() as u64 {
            p.page(PageStatus::More(Cursor::Opaque("truncated".into())));
        } else {
            p.page(PageStatus::Exhaustive);
        }
        p
    }

    fn summary_json_bytes(&self) -> Vec<u8> {
        let truncated = u64::from(self.total_results) > self.papers.len() as u64;
        let payload = serde_json::json!({
            "total_results": self.total_results,
            "listed_results": self.papers.len(),
            "truncated": truncated,
            "request_url": &self.request_url,
        });
        let mut bytes =
            serde_json::to_vec_pretty(&payload).expect("serializing json! is infallible");
        bytes.push(b'\n');
        bytes
    }
}

impl ListedPaper {
    /// Project everything we already have for this paper from the
    /// listing fetch: the paper dir, `metadata.json`/`links.json` at
    /// the paper root, two binary file entries, and the same skeleton
    /// for each known version. All version-specific data is a pure
    /// local function of the parsed entry.
    fn project_into(&self, p: &mut Projection, prefix: &str) {
        // `prefix` is always a non-empty FS-style path supplied by a
        // scope handler (e.g. `categories/{cat}/{ym}`).
        let paper_base = format!("{}/{}", prefix.trim_matches('/'), self.encoded_key);
        p.proj_dir(paper_base.clone());
        project_paper_files(p, &paper_base, &self.entry, None);

        let versions_index = format!("{paper_base}/versions");
        p.proj_dir(versions_index.clone());
        for version in 1..=self.entry.latest_version {
            let version_root = format!("{versions_index}/v{version}");
            p.proj_dir(version_root.clone());
            project_paper_files(p, &version_root, &self.entry, Some(version));
        }
    }
}

fn project_paper_files(p: &mut Projection, base: &str, entry: &ParsedEntry, version: Option<u32>) {
    p.proj(
        format!("{base}/metadata.json"),
        entry.metadata_json_bytes(version),
    );
    p.proj(
        format!("{base}/links.json"),
        entry.links_json_bytes(version),
    );
    p.proj_file(
        format!("{base}/paper.pdf"),
        FileProj::deferred(Size::Unknown, ReadMode::Full, Stability::Immutable),
    );
    p.proj_file(
        format!("{base}/source.tar.gz"),
        FileProj::deferred(Size::Unknown, ReadMode::Full, Stability::Immutable),
    );
}
