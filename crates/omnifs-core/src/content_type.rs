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
    /// `application/xml` (`.xml`).
    Xml,
    /// `application/octet-stream` (`.raw`) - the identity representation.
    Octet,
    /// `application/atom+xml` (`.atom`).
    Atom,
    /// An SDK-supplied type for a field or custom-suffix leaf the known suffix
    /// map cannot type. The host echoes the string verbatim.
    Custom(&'static str),
}

impl ContentType {
    /// Construct a custom content type after validating it with the `mime`
    /// parser. Existing code may still use [`ContentType::Custom`] directly
    /// when it already owns a trusted static MIME string.
    pub fn custom(value: &'static str) -> Result<Self, mime::FromStrError> {
        value.parse::<mime::Mime>()?;
        Ok(Self::Custom(value))
    }

    /// The representation extension this type renders to, if it has a known
    /// path suffix.
    pub fn extension(self) -> Option<&'static str> {
        match self {
            Self::Markdown => Some("md"),
            Self::Json => Some("json"),
            Self::Xml => Some("xml"),
            Self::Octet => Some("raw"),
            Self::Atom => Some("atom"),
            Self::Custom(_) => None,
        }
    }

    /// Map a known representation extension to its content type.
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            "md" => Some(Self::Markdown),
            "json" => Some(Self::Json),
            "xml" => Some(Self::Xml),
            "raw" => Some(Self::Octet),
            "atom" => Some(Self::Atom),
            _ => None,
        }
    }

    /// The MIME string carried on the wire.
    pub fn as_mime(self) -> &'static str {
        match self {
            Self::Markdown => "text/markdown",
            Self::Json => "application/json",
            Self::Xml => "application/xml",
            Self::Octet => "application/octet-stream",
            Self::Atom => "application/atom+xml",
            Self::Custom(s) => s,
        }
    }

    /// Parse this content type into the standard Rust [`mime::Mime`] type.
    pub fn to_mime(self) -> Result<mime::Mime, mime::FromStrError> {
        self.try_into()
    }

    /// Recover a [`ContentType`] from a MIME string.
    pub fn from_mime(mime: &str) -> Option<Self> {
        match mime {
            "text/markdown" => Some(Self::Markdown),
            "application/json" => Some(Self::Json),
            "application/xml" => Some(Self::Xml),
            "application/octet-stream" => Some(Self::Octet),
            "application/atom+xml" => Some(Self::Atom),
            _ => None,
        }
    }

    /// Recover a [`ContentType`] from a parsed MIME value. Parameters are not
    /// part of representation identity, so matching uses the MIME essence.
    pub fn from_mime_type(mime: &mime::Mime) -> Option<Self> {
        Self::from_mime(mime.essence_str())
    }
}

impl TryFrom<ContentType> for mime::Mime {
    type Error = mime::FromStrError;

    fn try_from(value: ContentType) -> Result<Self, Self::Error> {
        value.as_mime().parse()
    }
}

#[cfg(test)]
mod tests {
    use super::ContentType;

    #[test]
    fn custom_validates_mime_string() {
        assert_eq!(
            ContentType::custom("text/plain").unwrap(),
            ContentType::Custom("text/plain")
        );
        assert!(ContentType::custom("not-a-mime").is_err());
    }

    #[test]
    fn maps_known_extensions() {
        assert_eq!(
            ContentType::from_extension("md"),
            Some(ContentType::Markdown)
        );
        assert_eq!(ContentType::from_extension("atom"), Some(ContentType::Atom));
    }

    #[test]
    fn converts_to_mime_crate_type() {
        assert_eq!(ContentType::Json.to_mime().unwrap(), mime::APPLICATION_JSON);
        assert_eq!(
            ContentType::Custom("text/x-diff").to_mime().unwrap(),
            "text/x-diff".parse::<mime::Mime>().unwrap()
        );
    }

    #[test]
    fn recovers_known_type_from_parsed_mime_essence() {
        let markdown = "text/markdown; charset=utf-8"
            .parse::<mime::Mime>()
            .unwrap();

        assert_eq!(
            ContentType::from_mime_type(&markdown),
            Some(ContentType::Markdown)
        );
    }
}
