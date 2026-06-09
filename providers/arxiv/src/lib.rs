#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
#![allow(clippy::needless_pass_by_value)]

//! arxiv-provider: arXiv virtual filesystem provider for omnifs.

mod api;
mod objects;

use core::fmt;
use core::str::FromStr;

use crate::api::{
    ArxivApi, ArxivWeb, CATEGORY_PAGE_SIZE, download_pdf, download_source, fetch_category_page,
    load_paper,
};
use crate::objects::Paper;
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
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CategoryName(String);

impl FromStr for CategoryName {
    type Err = ProviderError;

    fn from_str(value: &str) -> Result<Self> {
        if value.is_empty()
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
        {
            return Err(ProviderError::not_found("invalid category"));
        }
        Ok(Self(value.to_string()))
    }
}

impl fmt::Display for CategoryName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl AsRef<str> for CategoryName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl PathSegment for CategoryName {
    fn choices() -> Option<&'static [&'static str]> {
        None
    }
}

/// Version directory segment (`@latest`, `v1`, `v2`, ...).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PaperVersion(Option<u32>);

impl PaperVersion {
    fn latest() -> Self {
        Self(None)
    }

    fn number(self) -> Option<u32> {
        self.0
    }

    fn is_numbered(self) -> bool {
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
    paper: PaperId,
}

#[omnifs_sdk::path_captures]
pub struct PaperVersionKey {
    paper: PaperId,
    version: Facet<PaperVersion>,
}

#[omnifs_sdk::path_captures]
pub struct CategoryKey {
    category: CategoryName,
}

#[omnifs_sdk::provider(
    metadata = "omnifs.provider.json",
    resources(endpoints = [ArxivApi, ArxivWeb]),
)]
impl ArxivProvider {
    fn start(r: &mut Router) -> Result<()> {
        let papers = object::<Paper>("/{paper}/{version}", |o| {
            o.representations("paper", ())?;
            o.file("paper.json").handler(PaperVersionKey::json)?;
            o.file("paper.pdf").handler(PaperVersionKey::pdf)?;
            o.file("source.tar.gz").handler(PaperVersionKey::source)?;
            Ok(())
        })?;

        r.attach("/papers", &papers)?;
        r.attach("/categories/{category}/papers", &papers)?;

        r.dir("/papers/{paper}").handler(PaperKey::versions)?;
        r.dir("/categories/{category}/papers/{paper}")
            .handler(PaperKey::versions)?;
        r.dir("/categories/{category}").handler(CategoryKey::sub)?;
        r.dir("/categories/{category}/papers")
            .handler(CategoryKey::recent)?;

        Ok(())
    }
}

impl Key for PaperVersionKey {
    type Object = Paper;
    type State = ();

    async fn load(&self, cx: &Cx, since: Option<Validator>) -> Result<Load<Paper>> {
        let loaded = load_paper(cx, self.paper.decoded(), since).await?;
        if let Load::Fresh { ref value, .. } = loaded
            && let Some(version) = self.version.number()
        {
            value.validate_version(version)?;
        }
        Ok(loaded)
    }
}

impl PaperKey {
    async fn versions(cx: DirCx, key: PaperKey) -> Result<DirProjection> {
        let paper = loaded_paper(&cx, &key).await?;
        Paper::version_dirs(&paper)
    }
}

impl PaperVersionKey {
    fn paper_key(&self) -> PaperKey {
        PaperKey {
            paper: self.paper.clone(),
        }
    }

    async fn pdf(cx: Cx, key: PaperVersionKey) -> Result<FileProjection> {
        let paper = loaded_paper(&cx, &key.paper_key()).await?;
        let version = key.version.number();
        if let Some(version) = version {
            paper.validate_version(version)?;
        }
        let blob = download_pdf(&cx, key.paper.decoded(), version).await?;
        let builder = FileProjection::blob(blob.id).size(Size::Exact(blob.size));
        Ok(if key.version.is_numbered() {
            builder.immutable().build()
        } else {
            builder.mutable().build()
        })
    }

    async fn source(cx: Cx, key: PaperVersionKey) -> Result<FileProjection> {
        let paper = loaded_paper(&cx, &key.paper_key()).await?;
        let version = key.version.number();
        if let Some(version) = version {
            paper.validate_version(version)?;
        }
        let blob = download_source(&cx, key.paper.decoded(), version).await?;
        let builder = FileProjection::blob(blob.id).size(Size::Exact(blob.size));
        Ok(if key.version.is_numbered() {
            builder.immutable().build()
        } else {
            builder.mutable().build()
        })
    }

    async fn json(cx: Cx, key: PaperVersionKey) -> Result<FileProjection> {
        let paper = loaded_paper(&cx, &key.paper_key()).await?;
        if let Some(version) = key.version.number() {
            paper.validate_version(version)?;
        }
        let builder = FileProjection::body(paper.metadata_json_bytes(key.version.number())?)
            .content_type(ContentType::Json);
        Ok(if key.version.is_numbered() {
            builder.immutable().build()
        } else {
            builder.mutable().build()
        })
    }
}

impl CategoryKey {
    #[allow(clippy::unused_async)]
    async fn sub(_cx: DirCx, _key: CategoryKey) -> Result<DirProjection> {
        Ok(DirProjection::exhaustive([Entry::dir("papers")]))
    }

    async fn recent(cx: DirCx, key: CategoryKey) -> Result<DirProjection> {
        let page = match cx.cursor() {
            Some(Cursor::Page(n)) => *n,
            _ => 0,
        };
        let ids = fetch_category_page(&cx, key.category.as_ref(), page).await?;
        let exhaustive = ids.len() < CATEGORY_PAGE_SIZE as usize;
        let entries = ids.into_iter().filter_map(|raw| {
            PaperId::from_decoded(&raw)
                .ok()
                .map(|id| Entry::dir(id.to_string()))
        });
        if exhaustive {
            Ok(DirProjection::open(entries))
        } else {
            Ok(DirProjection::paged(entries, Cursor::Page(page + 1)))
        }
    }
}

async fn loaded_paper(cx: &Cx, key: &PaperKey) -> Result<Paper> {
    let version_key = PaperVersionKey {
        paper: key.paper.clone(),
        version: Facet(PaperVersion::latest()),
    };
    match version_key.load(cx, None).await? {
        Load::Fresh { value, .. } => Ok(value),
        Load::Unchanged => Err(ProviderError::internal(
            "paper unchanged without a host-pushed canonical in this handler path",
        )),
        Load::NotFound => Err(ProviderError::not_found("paper not found")),
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
