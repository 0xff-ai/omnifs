#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

//! arxiv-provider: arXiv virtual filesystem provider for omnifs.

mod api;
mod objects;

use core::fmt;
use core::str::FromStr;

use crate::api::{CATEGORY_PAGE_SIZE, fetch_category_page};
use crate::objects::{Paper, loaded_paper};
use omnifs_sdk::prelude::*;
use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};

/// Percent-encode `/` so old-style ids are a single path segment.
const SEGMENT_ENCODE: &AsciiSet = &CONTROLS.add(b'/').add(b'%');

/// Decoded arXiv base id (e.g. `2401.12345`, `cs.LG/0512345`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PaperId(String);

impl PaperId {
    fn is_valid_decoded(value: &str) -> bool {
        if value.is_empty() {
            return false;
        }
        let has_digit = value.bytes().any(|b| b.is_ascii_digit());
        let has_separator = value.contains('.') || value.contains('/');
        has_digit
            && has_separator
            && value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
            })
    }

    pub(crate) fn decoded(&self) -> &str {
        &self.0
    }

    pub(crate) fn from_decoded(s: &str) -> Result<Self> {
        Self::is_valid_decoded(s)
            .then(|| Self(s.to_string()))
            .ok_or_else(|| ProviderError::invalid_input("invalid arXiv paper id"))
    }
}

impl FromStr for PaperId {
    type Err = ProviderError;

    fn from_str(segment: &str) -> Result<Self> {
        let decoded = percent_decode_str(segment)
            .decode_utf8()
            .map_err(|_| ProviderError::not_found("paper id encoding was not valid UTF-8"))?;
        if decoded.is_empty() {
            return Err(ProviderError::not_found("paper id is empty"));
        }
        let (base, explicit_version) = split_versioned_id(&decoded);
        if explicit_version.is_some() {
            return Err(ProviderError::not_found(
                "versioned paper ids must be accessed through a version selector",
            ));
        }
        Self::from_decoded(&base)
    }
}

impl fmt::Display for PaperId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            utf8_percent_encode(self.0.as_str(), SEGMENT_ENCODE)
        )
    }
}

impl PathSegment for PaperId {
    fn choices() -> Option<&'static [&'static str]> {
        None
    }
}

/// Validated arXiv category code.
#[omnifs_sdk::path_segment(validate = is_valid_category_name)]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CategoryName(String);

fn is_valid_category_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
}

/// Version directory segment (`@latest`, `v1`, `v2`, ...).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PaperVersion(Option<u32>);

impl PaperVersion {
    pub(crate) fn latest() -> Self {
        Self(None)
    }

    pub(crate) fn number(self) -> Option<u32> {
        self.0
    }

    pub(crate) fn is_numbered(self) -> bool {
        self.0.is_some()
    }
}

impl FromStr for PaperVersion {
    type Err = ProviderError;

    fn from_str(segment: &str) -> Result<Self> {
        if segment == "@latest" {
            return Ok(Self::latest());
        }
        let digits = segment
            .strip_prefix('v')
            .filter(|tail| !tail.is_empty())
            .ok_or_else(|| ProviderError::not_found("invalid paper version"))?;
        let number: u32 = digits
            .parse()
            .map_err(|_| ProviderError::not_found("invalid paper version"))?;
        if number == 0 {
            return Err(ProviderError::not_found("invalid paper version"));
        }
        Ok(Self(Some(number)))
    }
}

impl fmt::Display for PaperVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(version) => write!(f, "v{version}"),
            None => f.write_str("@latest"),
        }
    }
}

impl PathSegment for PaperVersion {
    fn choices() -> Option<&'static [&'static str]> {
        None
    }
}

#[omnifs_sdk::path_captures]
pub struct PaperKey {
    pub(crate) paper: PaperId,
}

#[omnifs_sdk::path_captures]
pub struct PaperVersionKey {
    pub(crate) paper: PaperId,
    pub(crate) version: Facet<PaperVersion>,
}

impl PaperVersionKey {
    pub(crate) fn paper_key(&self) -> PaperKey {
        PaperKey {
            paper: self.paper.clone(),
        }
    }
}

#[omnifs_sdk::path_captures]
pub struct CategoryKey {
    category: CategoryName,
}

#[omnifs_sdk::provider(
    id = "arxiv",
    display_name = "arXiv",
    description = "arXiv papers and abstracts as files",
    mount = "arxiv",
    capabilities(
        domain(
            "export.arxiv.org",
            "Fetch arXiv API metadata and paper resources from arXiv-owned domains."
        ),
        domain(
            "arxiv.org",
            "Fetch arXiv API metadata and paper resources from arXiv-owned domains."
        ),
    ),
    limits(memory_mb(
        64,
        "Bound provider memory while leaving enough room for Atom feeds and paper metadata."
    ),)
)]
impl ArxivProvider {
    fn start(r: &mut Router) -> Result<()> {
        let papers = r.object::<Paper>("/papers/{paper}/{version}", |o| {
            o.stability(|key| {
                if key.version.is_numbered() {
                    Stability::Stable
                } else {
                    Stability::Dynamic
                }
            });
            o.file("paper.atom").canonical::<Atom>()?;
            o.file("paper.json").computed(Paper::metadata_json)?;
            o.file("paper.pdf").blob(Paper::pdf)?;
            o.file("source.tar.gz").blob(Paper::source)?;
            Ok(())
        })?;

        r.alias("/categories/{category}/papers/{paper}/{version}", &papers)?;

        r.dir("/papers/{paper}").handler(PaperKey::versions)?;
        r.dir("/categories/{category}/papers/{paper}")
            .handler(PaperKey::versions)?;
        r.dir("/categories/{category}").handler(CategoryKey::sub)?;
        r.dir("/categories/{category}/papers")
            .handler(CategoryKey::recent)?;

        Ok(())
    }
}

impl PaperKey {
    async fn versions(cx: DirCx, key: PaperKey) -> Result<DirListing> {
        let paper = loaded_paper(&cx, &key).await?;
        Paper::version_dirs(&paper)
    }
}

impl CategoryKey {
    #[allow(clippy::unused_async)]
    async fn sub(_cx: DirCx, _key: CategoryKey) -> Result<DirListing> {
        Ok(DirListing::exhaustive([Entry::dir("papers")]))
    }

    async fn recent(cx: DirCx, key: CategoryKey) -> Result<DirListing> {
        let page = cx.page_cursor(0);
        let ids = fetch_category_page(&cx, key.category.as_ref(), page).await?;
        let exhaustive = ids.len() < CATEGORY_PAGE_SIZE as usize;
        let entries = ids.into_iter().filter_map(|raw| {
            PaperId::from_decoded(&raw)
                .ok()
                .map(|id| Entry::dir(id.to_string()))
        });
        if exhaustive {
            Ok(DirListing::open(entries))
        } else {
            Ok(DirListing::paged(entries, Cursor::Page(page + 1)))
        }
    }
}

pub(crate) fn normalize_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) fn split_versioned_id(raw_id: &str) -> (String, Option<u32>) {
    let bytes = raw_id.as_bytes();
    let mut split = bytes.len();
    while split > 0 && bytes[split - 1].is_ascii_digit() {
        split -= 1;
    }
    if split == bytes.len() || split == 0 || bytes[split - 1] != b'v' {
        return (raw_id.to_string(), None);
    }
    match raw_id[split..].parse::<u32>() {
        Ok(version) => (raw_id[..split - 1].to_string(), Some(version)),
        Err(_) => (raw_id.to_string(), None),
    }
}
