//! Multi-format object representations: render one canonical payload into
//! several leaf files (`item.md`, `item.json`, ...).
//!
//! The model: an object's canonical bytes are stored verbatim; each
//! representation leaf either serves those bytes as-is (the source content
//! type) or re-renders them through a [`RenderFn`]. Renders are pure
//! functions of the canonical bytes, so the host can re-serve every
//! representation from cache with no upstream call. Leaf names come from
//! the representation stem plus each content type's extension, which is
//! why [`RenderTable::build`] rejects extension collisions.

use crate::error::{ProviderError, Result};
use omnifs_core::ContentType;

/// A content-type marker (zero-size). `CT` is the wire content type this format
/// renders to. A thin type-level layer over `ContentType`.
pub trait Format {
    const CT: ContentType;
}

pub struct Json;
impl Format for Json {
    const CT: ContentType = ContentType::Json;
}

pub struct Markdown;
impl Format for Markdown {
    const CT: ContentType = ContentType::Markdown;
}

pub struct Octet;
impl Format for Octet {
    const CT: ContentType = ContentType::Octet;
}

pub struct Atom;
impl Format for Atom {
    const CT: ContentType = ContentType::Atom;
}

pub struct Yaml;
impl Format for Yaml {
    const CT: ContentType = ContentType::Yaml;
}

pub struct Html;
impl Format for Html {
    const CT: ContentType = ContentType::Custom("text/html");
}

pub struct Diff;
impl Format for Diff {
    const CT: ContentType = ContentType::Custom("text/x-diff");
}

/// Build erased render fns from format markers for object `O`.
///
/// Implemented only for the fixed tuples `()`, `(Markdown,)`,
/// `(Markdown, Html)`, and `(Markdown, Html, Diff)`; each non-unit element
/// requires `O: Representable<F>`. `()` means the object exposes only its
/// canonical representation. A new combination needs a new
/// `impl_render_set!` line in this module; it cannot be assembled at a
/// provider call site.
pub trait RenderSet<O: crate::object::Object> {
    fn register(table: &mut Vec<(ContentType, RenderFn)>);
}

impl<O: crate::object::Object> RenderSet<O> for () {
    fn register(_table: &mut Vec<(ContentType, RenderFn)>) {}
}

macro_rules! impl_render_set {
    ($($F:ident),+) => {
        impl<O: crate::object::Object $(+ Representable<$F>)*> RenderSet<O> for ($($F,)+) {
            fn register(table: &mut Vec<(ContentType, RenderFn)>) {
                $(table.push((<$F as Format>::CT, render_fn::<O, $F>()));)+
            }
        }
    };
}

impl_render_set!(Markdown);
impl_render_set!(Yaml);
impl_render_set!(Markdown, Html);
impl_render_set!(Markdown, Html, Diff);

fn render_fn<O, F>() -> RenderFn
where
    O: crate::object::Object + Representable<F>,
    F: Format,
{
    |canonical| O::parse_canonical(canonical).map(|value| value.represent())
}

/// A value that can render itself into format `F`.
///
/// Rendering is infallible by design: parse failures belong to
/// `Object::parse_canonical`, and by the time `represent` runs the value
/// is already a well-formed `O`. Keep it a pure function of `self`; the
/// output may be served from cache long after the render ran.
///
/// ```ignore
/// impl Representable<Markdown> for Issue {
///     fn represent(&self) -> Vec<u8> {
///         format!("# {}\n\n{}\n", self.title, self.body).into_bytes()
///     }
/// }
/// ```
pub trait Representable<F: Format> {
    fn represent(&self) -> Vec<u8>;
}

/// Erased render function: canonical bytes in, rendered bytes out
/// (parse the canonical, then [`Representable::represent`]).
pub type RenderFn = fn(&[u8]) -> Result<Vec<u8>>;

/// Per-route representation dispatch: one verbatim source (canonical) content
/// type plus a set of erased renders keyed by content type. Built once at
/// registration; consulted on each read.
#[derive(Clone)]
pub struct RenderTable {
    pub(crate) source_ct: ContentType,
    pub(crate) renders: Vec<(ContentType, RenderFn)>,
}

impl RenderTable {
    /// Build from the canonical/source CT and the derived renders. Rejects (with
    /// `ProviderError::invalid_input`) any of: a render whose CT equals
    /// `source_ct`; a duplicate render CT; or two CTs (source or render) that
    /// share the same `extension()` (an ambiguous leaf extension).
    pub fn build(source_ct: ContentType, renders: Vec<(ContentType, RenderFn)>) -> Result<Self> {
        // One invariant in one pass: the source CT and every render CT are
        // pairwise distinct (this subsumes "a render must not equal the source"
        // and "no duplicate render CT"), and no two share a standard extension
        // (which would make their leaf names collide).
        let mut seen_cts: Vec<ContentType> = Vec::with_capacity(renders.len() + 1);
        let mut seen_exts: Vec<&'static str> = Vec::new();
        for ct in std::iter::once(source_ct).chain(renders.iter().map(|(ct, _)| *ct)) {
            if seen_cts.contains(&ct) {
                return Err(ProviderError::invalid_input(format!(
                    "duplicate representation content type {ct:?}"
                )));
            }
            seen_cts.push(ct);
            if let Some(ext) = ct.extension() {
                if seen_exts.contains(&ext) {
                    return Err(ProviderError::invalid_input(format!(
                        "two representations share the leaf extension .{ext}"
                    )));
                }
                seen_exts.push(ext);
            }
        }

        Ok(Self { source_ct, renders })
    }

    /// Serve content type `ct` from `canonical`: if `ct == source_ct`, return the
    /// canonical bytes verbatim (no parse); otherwise invoke the matching render
    /// fn. If `ct` is neither the source nor a registered render, return
    /// `ProviderError::not_found`.
    pub fn serve(&self, ct: ContentType, canonical: &[u8]) -> Result<Vec<u8>> {
        if ct == self.source_ct {
            return Ok(canonical.to_vec());
        }
        for (render_ct, render) in &self.renders {
            if *render_ct == ct {
                return render(canonical);
            }
        }
        Err(ProviderError::not_found(format!(
            "no render registered for content type {ct:?}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upper(canonical: &[u8]) -> Result<Vec<u8>> {
        let source = std::str::from_utf8(canonical)
            .map_err(|error| ProviderError::invalid_input(format!("utf-8 decode: {error}")))?;
        Ok(source.to_ascii_uppercase().into_bytes())
    }

    #[test]
    fn serve_source_is_verbatim() {
        let table =
            RenderTable::build(ContentType::Json, vec![(ContentType::Markdown, upper)]).unwrap();
        let raw = br"{raw}";
        assert_eq!(table.serve(ContentType::Json, raw).unwrap(), raw);
    }

    #[test]
    fn serve_render_invokes_fn() {
        let table =
            RenderTable::build(ContentType::Json, vec![(ContentType::Markdown, upper)]).unwrap();
        assert_eq!(
            table.serve(ContentType::Markdown, b"hi").unwrap(),
            b"HI".as_slice()
        );
    }

    #[test]
    fn serve_unknown_ct_errors() {
        let table =
            RenderTable::build(ContentType::Json, vec![(ContentType::Markdown, upper)]).unwrap();
        assert!(table.serve(ContentType::Octet, b"x").is_err());
    }

    #[test]
    fn build_rejects_duplicate_render_ct() {
        let result = RenderTable::build(
            ContentType::Json,
            vec![
                (ContentType::Markdown, upper),
                (ContentType::Markdown, upper),
            ],
        );
        assert_eq!(
            result.err().map(|e| e.kind()),
            Some(crate::error::ProviderErrorKind::InvalidInput),
        );
    }

    #[test]
    fn build_rejects_render_equal_source() {
        let result = RenderTable::build(ContentType::Json, vec![(ContentType::Json, upper)]);
        assert_eq!(
            result.err().map(|e| e.kind()),
            Some(crate::error::ProviderErrorKind::InvalidInput),
        );
    }
}
