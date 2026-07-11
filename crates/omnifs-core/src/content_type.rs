//! Content types that select object representations.
//!
//! The host can infer these types from known representation suffixes. For a
//! bare-name leaf, the SDK supplies the content type on the directory entry and
//! the host echoes it opaquely.

/// The MIME type that selects an object representation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentType {
    /// `text/markdown` (`.md`).
    Markdown,
    /// `application/json` (`.json`).
    Json,
    /// `application/yaml` (`.yaml`).
    Yaml,
    /// `application/xml` (`.xml`).
    Xml,
    /// `application/octet-stream` (`.raw`) - the identity representation.
    Octet,
    /// `application/atom+xml` (`.atom`).
    Atom,
    /// `text/plain` (`.txt`).
    Text,
    /// An SDK-supplied type for a field or custom-suffix leaf the known suffix
    /// map cannot type. The host echoes the string verbatim.
    Custom(&'static str),
}

impl ContentType {
    /// The representation extension this type renders to, if it has a known
    /// path suffix.
    pub fn extension(self) -> Option<&'static str> {
        match self {
            Self::Markdown => Some("md"),
            Self::Json => Some("json"),
            Self::Yaml => Some("yaml"),
            Self::Xml => Some("xml"),
            Self::Octet => Some("raw"),
            Self::Atom => Some("atom"),
            Self::Text => Some("txt"),
            Self::Custom(_) => None,
        }
    }

    /// Map a known representation extension to its content type.
    pub(crate) fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            "md" => Some(Self::Markdown),
            "json" => Some(Self::Json),
            "yaml" | "yml" => Some(Self::Yaml),
            "xml" => Some(Self::Xml),
            "raw" => Some(Self::Octet),
            "atom" => Some(Self::Atom),
            "txt" => Some(Self::Text),
            _ => None,
        }
    }

    /// The MIME string carried on the wire.
    pub fn as_mime(self) -> &'static str {
        match self {
            Self::Markdown => "text/markdown",
            Self::Json => "application/json",
            Self::Yaml => "application/yaml",
            Self::Xml => "application/xml",
            Self::Octet => "application/octet-stream",
            Self::Atom => "application/atom+xml",
            Self::Text => "text/plain",
            Self::Custom(s) => s,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ContentType;

    #[test]
    fn maps_known_extensions() {
        assert_eq!(
            ContentType::from_extension("md"),
            Some(ContentType::Markdown)
        );
        assert_eq!(ContentType::from_extension("atom"), Some(ContentType::Atom));
        assert_eq!(ContentType::from_extension("yaml"), Some(ContentType::Yaml));
        assert_eq!(ContentType::from_extension("yml"), Some(ContentType::Yaml));
    }
}
