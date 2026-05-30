#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

//! web-provider: fetch any URL and project it as Markdown.
//!
//! Every path segment under the `/web` mount is a component of a URL. The
//! rendered Markdown of a page is served from an `index.md` file inside the
//! directory that names it:
//!
//! ```text
//! cat /web/example.com/index.md                  # https://example.com
//! cat /web/example.com/blog/post/index.md        # https://example.com/blog/post
//! ```
//!
//! HTML is converted with the `htmd` engine (a turndown.js-compatible,
//! pure-Rust HTML→Markdown converter) which produces clean, LLM-friendly
//! Markdown. `script`/`style` noise is stripped before conversion.

use std::str::FromStr;

use htmd::HtmlToMarkdown;
use omnifs_sdk::Cx;
use omnifs_sdk::http::ResponseExt;
use omnifs_sdk::prelude::*;

/// Reserved file name that exposes the Markdown rendering of a directory's URL.
const INDEX_NAME: &str = "index.md";
/// Reserved file name at the mount root that documents usage.
const README_NAME: &str = "README";

const README: &str = "\
# omnifs web provider

Fetch any URL and read it back as Markdown.

Each directory under /web is a component of a URL, with the host first. The
rendered Markdown of a page is the `index.md` file inside its directory:

    cat /web/example.com/index.md                 # https://example.com
    cat /web/example.com/blog/post/index.md       # https://example.com/blog/post
    cat /web/en.wikipedia.org/wiki/Rust/index.md  # https://en.wikipedia.org/wiki/Rust

Navigate with the usual tools; `ls` always shows the directory's index.md:

    cd /web/example.com && ls

Notes:
- Only https is fetched (the runtime denies plain http and private addresses).
- Conversion uses the htmd HTML->Markdown engine; script/style noise is stripped.
- Up to the host plus 7 path segments are addressable.
";

#[omnifs_sdk::config]
#[derive(Clone)]
struct Config {
    /// URL scheme prepended to the requested path. Only `https` is permitted
    /// by the runtime sandbox; exposed for completeness.
    #[serde(default = "default_scheme")]
    scheme: String,
}

fn default_scheme() -> String {
    String::from("https")
}

#[derive(Clone)]
struct State {
    scheme: String,
}

/// One URL path component. Rejects the reserved file names and relative
/// markers so that `index.md`/`README` resolve as files rather than being
/// captured as navigable directories.
#[derive(Clone, Debug)]
struct Seg(String);

impl FromStr for Seg {
    type Err = ();

    fn from_str(value: &str) -> core::result::Result<Self, Self::Err> {
        if value.is_empty()
            || value == "."
            || value == ".."
            || value == INDEX_NAME
            || value == README_NAME
        {
            return Err(());
        }
        Ok(Self(value.to_string()))
    }
}

/// A directory whose concrete children are discovered by lookup, never
/// enumerated. Marking it non-exhaustive keeps the host from negatively
/// caching unseen URL segments.
fn dynamic_dir() -> Projection {
    let mut projection = Projection::new();
    projection.page(PageStatus::More(Cursor::Opaque("web".to_string())));
    projection
}

/// Fetch `https://<segments joined by '/'>` and return its Markdown rendering.
async fn fetch_markdown(cx: &Cx<State>, segments: &[&str]) -> Result<FileContent> {
    let scheme = cx.state(|state| state.scheme.clone());
    let url = format!("{scheme}://{}", segments.join("/"));

    let response = cx
        .http()
        .get(url)
        .header("User-Agent", "omnifs-provider-web/0.1.0")
        .header("Accept", "text/html,application/xhtml+xml,text/plain")
        .send()
        .await?
        .error_for_status()?;
    let body = response.into_body();

    let html = String::from_utf8_lossy(&body);
    let markdown = HtmlToMarkdown::builder()
        .skip_tags(vec!["script", "style", "noscript"])
        .build()
        .convert(&html)
        .map_err(|error| {
            ProviderError::internal(format!("HTML to Markdown conversion failed: {error}"))
        })?;

    let mut markdown = markdown;
    if !markdown.ends_with('\n') {
        markdown.push('\n');
    }
    Ok(FileContent::bytes(markdown.into_bytes()))
}

pub struct WebHandlers;

#[handlers]
impl WebHandlers {
    #[dir("/")]
    fn root() -> Result<Projection> {
        // `README` merges in as a static child of the root listing.
        Ok(dynamic_dir())
    }

    #[file("/README")]
    fn readme() -> Result<FileContent> {
        Ok(FileContent::bytes(README))
    }

    // Navigable URL-prefix directories. Each depth needs its own handler so
    // that the segment at that depth is addressable as a directory; the
    // `Seg` validator rejects `index.md`/`README` so those resolve as files.
    #[dir("/{s1}")]
    fn d1(_s1: Seg) -> Result<Projection> {
        Ok(dynamic_dir())
    }

    #[dir("/{s1}/{s2}")]
    fn d2(_s1: Seg, _s2: Seg) -> Result<Projection> {
        Ok(dynamic_dir())
    }

    #[dir("/{s1}/{s2}/{s3}")]
    fn d3(_s1: Seg, _s2: Seg, _s3: Seg) -> Result<Projection> {
        Ok(dynamic_dir())
    }

    #[dir("/{s1}/{s2}/{s3}/{s4}")]
    fn d4(_s1: Seg, _s2: Seg, _s3: Seg, _s4: Seg) -> Result<Projection> {
        Ok(dynamic_dir())
    }

    #[dir("/{s1}/{s2}/{s3}/{s4}/{s5}")]
    fn d5(_s1: Seg, _s2: Seg, _s3: Seg, _s4: Seg, _s5: Seg) -> Result<Projection> {
        Ok(dynamic_dir())
    }

    #[dir("/{s1}/{s2}/{s3}/{s4}/{s5}/{s6}")]
    fn d6(_s1: Seg, _s2: Seg, _s3: Seg, _s4: Seg, _s5: Seg, _s6: Seg) -> Result<Projection> {
        Ok(dynamic_dir())
    }

    #[dir("/{s1}/{s2}/{s3}/{s4}/{s5}/{s6}/{s7}")]
    fn d7(
        _s1: Seg,
        _s2: Seg,
        _s3: Seg,
        _s4: Seg,
        _s5: Seg,
        _s6: Seg,
        _s7: Seg,
    ) -> Result<Projection> {
        Ok(dynamic_dir())
    }

    #[dir("/{s1}/{s2}/{s3}/{s4}/{s5}/{s6}/{s7}/{s8}")]
    fn d8(
        _s1: Seg,
        _s2: Seg,
        _s3: Seg,
        _s4: Seg,
        _s5: Seg,
        _s6: Seg,
        _s7: Seg,
        _s8: Seg,
    ) -> Result<Projection> {
        Ok(dynamic_dir())
    }

    // The Markdown rendering for each directory's URL.
    #[file("/{s1}/index.md")]
    async fn f1(cx: &Cx<State>, s1: Seg) -> Result<FileContent> {
        fetch_markdown(cx, &[&s1.0]).await
    }

    #[file("/{s1}/{s2}/index.md")]
    async fn f2(cx: &Cx<State>, s1: Seg, s2: Seg) -> Result<FileContent> {
        fetch_markdown(cx, &[&s1.0, &s2.0]).await
    }

    #[file("/{s1}/{s2}/{s3}/index.md")]
    async fn f3(cx: &Cx<State>, s1: Seg, s2: Seg, s3: Seg) -> Result<FileContent> {
        fetch_markdown(cx, &[&s1.0, &s2.0, &s3.0]).await
    }

    #[file("/{s1}/{s2}/{s3}/{s4}/index.md")]
    async fn f4(cx: &Cx<State>, s1: Seg, s2: Seg, s3: Seg, s4: Seg) -> Result<FileContent> {
        fetch_markdown(cx, &[&s1.0, &s2.0, &s3.0, &s4.0]).await
    }

    #[file("/{s1}/{s2}/{s3}/{s4}/{s5}/index.md")]
    async fn f5(
        cx: &Cx<State>,
        s1: Seg,
        s2: Seg,
        s3: Seg,
        s4: Seg,
        s5: Seg,
    ) -> Result<FileContent> {
        fetch_markdown(cx, &[&s1.0, &s2.0, &s3.0, &s4.0, &s5.0]).await
    }

    #[file("/{s1}/{s2}/{s3}/{s4}/{s5}/{s6}/index.md")]
    async fn f6(
        cx: &Cx<State>,
        s1: Seg,
        s2: Seg,
        s3: Seg,
        s4: Seg,
        s5: Seg,
        s6: Seg,
    ) -> Result<FileContent> {
        fetch_markdown(cx, &[&s1.0, &s2.0, &s3.0, &s4.0, &s5.0, &s6.0]).await
    }

    #[file("/{s1}/{s2}/{s3}/{s4}/{s5}/{s6}/{s7}/index.md")]
    async fn f7(
        cx: &Cx<State>,
        s1: Seg,
        s2: Seg,
        s3: Seg,
        s4: Seg,
        s5: Seg,
        s6: Seg,
        s7: Seg,
    ) -> Result<FileContent> {
        fetch_markdown(cx, &[&s1.0, &s2.0, &s3.0, &s4.0, &s5.0, &s6.0, &s7.0]).await
    }

    #[file("/{s1}/{s2}/{s3}/{s4}/{s5}/{s6}/{s7}/{s8}/index.md")]
    async fn f8(
        cx: &Cx<State>,
        s1: Seg,
        s2: Seg,
        s3: Seg,
        s4: Seg,
        s5: Seg,
        s6: Seg,
        s7: Seg,
        s8: Seg,
    ) -> Result<FileContent> {
        fetch_markdown(
            cx,
            &[&s1.0, &s2.0, &s3.0, &s4.0, &s5.0, &s6.0, &s7.0, &s8.0],
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The file/directory disambiguation depends entirely on `Seg` rejecting
    // the reserved file names: if `index.md` (or `README`) parsed as a
    // navigable directory it would shadow the file route at lookup time, and
    // the page would never be readable.
    #[test]
    fn seg_rejects_reserved_and_relative_names() {
        assert!("index.md".parse::<Seg>().is_err());
        assert!("README".parse::<Seg>().is_err());
        assert!(".".parse::<Seg>().is_err());
        assert!("..".parse::<Seg>().is_err());
        assert!("".parse::<Seg>().is_err());
    }

    #[test]
    fn seg_accepts_url_components() {
        for value in ["example.com", "blog", "post", "search?q=rust", "Rust"] {
            assert_eq!(value.parse::<Seg>().expect("valid segment").0, value);
        }
    }
}

#[provider(metadata = "omnifs.provider.json", mounts(crate::WebHandlers))]
impl WebProvider {
    fn init(config: Config) -> (State, ProviderInfo, RequestedCapabilities) {
        (
            State {
                scheme: config.scheme,
            },
            ProviderInfo {
                name: "web-provider".to_string(),
                version: "0.1.0".to_string(),
                description: "Fetches any URL and projects it as Markdown".to_string(),
            },
            RequestedCapabilities::runtime_only(0),
        )
    }
}
