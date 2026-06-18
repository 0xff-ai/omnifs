use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::ffi::OsStr;
use std::fmt;
use std::ops::Deref;

use crate::ContentType;

// Note: Pattern, CaptureLocation, and the route-pattern machinery live in
// crates/omnifs-sdk/src/router/pattern.rs — they are SDK-only types.

/// A validated omnifs protocol path.
///
/// A path is absolute, uses `/` as the only separator, has no trailing slash
/// except root, has no empty segments, and never contains `.` or `..`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Path(String);

/// A validated single protocol path segment.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Segment(String);

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("empty path")]
    Empty,
    #[error("path is not absolute: {0:?}")]
    MissingLeadingSlash(String),
    #[error("double slash in path: {0:?}")]
    DoubleSlash(String),
    #[error("trailing slash on non-root path: {0:?}")]
    TrailingSlash(String),
    #[error("empty path segment")]
    EmptySegment,
    #[error("path contains `.` or `..` segment: {0:?}")]
    RelativeSegment(String),
    #[error("name segment contains `/`: {0:?}")]
    SlashInSegment(String),
    #[error("path segment contains a control character: {0:?}")]
    ControlCharInSegment(String),
    #[error("path segment is not valid UTF-8")]
    NonUtf8Segment,
}

impl Path {
    pub const ROOT: &'static str = "/";

    /// Construct from a string already known to satisfy the protocol path
    /// invariants.
    pub fn from_validated(path: impl Into<String>) -> Self {
        let path = path.into();
        debug_assert!(
            Self::validate_str(&path).is_ok(),
            "Path::from_validated received an invalid protocol path: {path:?}"
        );
        Self(path)
    }

    pub fn root() -> Self {
        Self::from_validated(Self::ROOT)
    }

    pub fn parse(path: &str) -> Result<Self, ParseError> {
        Self::validate_str(path)?;
        Ok(Self(path.to_string()))
    }

    /// Parse a batch of wire path strings, short-circuiting on the first
    /// invalid entry. The returned `ParseError` names which validation failed
    /// (and, for most variants, the offending path), so callers need not carry
    /// the raw string alongside it.
    pub fn parse_all(paths: &[String]) -> Result<Vec<Self>, ParseError> {
        paths.iter().map(|path| Self::parse(path)).collect()
    }

    pub fn validate_str(path: &str) -> Result<(), ParseError> {
        if path.is_empty() {
            return Err(ParseError::Empty);
        }
        if !path.starts_with('/') {
            return Err(ParseError::MissingLeadingSlash(path.to_string()));
        }
        if path == Self::ROOT {
            return Ok(());
        }
        if path.ends_with('/') {
            return Err(ParseError::TrailingSlash(path.to_string()));
        }
        if path.contains("//") {
            return Err(ParseError::DoubleSlash(path.to_string()));
        }
        for segment in path[1..].split('/') {
            validate_segment_str(segment)?;
        }
        Ok(())
    }

    pub fn join(&self, name: &str) -> Result<Self, ParseError> {
        let segment = Segment::try_from(name)?;
        Ok(self.join_segment(&segment))
    }

    #[must_use]
    pub fn join_segment(&self, segment: &Segment) -> Self {
        if self.is_root() {
            Self::from_validated(format!("/{}", segment.as_str()))
        } else {
            Self::from_validated(format!("{}/{}", self.0, segment.as_str()))
        }
    }

    pub fn parent(&self) -> Option<Self> {
        if self.is_root() {
            return None;
        }
        let (parent, _) = self.0.rsplit_once('/')?;
        if parent.is_empty() {
            Some(Self::root())
        } else {
            Some(Self::from_validated(parent))
        }
    }

    pub fn parent_and_name(&self) -> Option<(Self, &str)> {
        if self.is_root() {
            return None;
        }
        let (parent, name) = self.0.rsplit_once('/')?;
        let parent = if parent.is_empty() {
            Self::root()
        } else {
            Self::from_validated(parent)
        };
        Some((parent, name))
    }

    pub fn name(&self) -> &str {
        if self.is_root() {
            ""
        } else {
            self.0.rsplit('/').next().unwrap_or("")
        }
    }

    /// Infer the representation content type selected by this path's leaf
    /// extension, if the extension is known to omnifs.
    pub fn content_type(&self) -> Option<ContentType> {
        self.name()
            .rsplit_once('.')
            .and_then(|(_, ext)| ContentType::from_extension(ext))
    }

    /// Return the MIME string this path carries into provider `read-file`.
    ///
    /// Known representation extensions win. Otherwise the caller-supplied
    /// stored content type is echoed, falling back to octet-stream.
    pub fn content_type_mime<'a>(&self, stored: Option<&'a str>) -> &'a str {
        if let Some(content_type) = self.content_type() {
            content_type.as_mime()
        } else if let Some(stored) = stored {
            stored
        } else {
            ContentType::Octet.as_mime()
        }
    }

    pub fn is_root(&self) -> bool {
        self.0 == Self::ROOT
    }

    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0[1..].split('/').filter(|segment| !segment.is_empty())
    }

    pub fn has_prefix(&self, prefix: &Self) -> bool {
        if prefix.is_root() || self == prefix {
            return true;
        }
        self.0
            .strip_prefix(prefix.as_str())
            .is_some_and(|suffix| suffix.starts_with('/'))
    }

    pub fn strip_prefix(&self, prefix: &Self) -> Option<Self> {
        if !self.has_prefix(prefix) {
            return None;
        }
        if prefix.is_root() {
            return Some(self.clone());
        }
        if self == prefix {
            return Some(Self::root());
        }
        let suffix = self.0.strip_prefix(prefix.as_str())?;
        Some(Self::from_validated(suffix))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for Path {
    fn default() -> Self {
        Self::root()
    }
}

impl Deref for Path {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AsRef<str> for Path {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl From<Path> for String {
    fn from(path: Path) -> Self {
        path.0
    }
}

impl fmt::Display for Path {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for Path {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Path {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

impl Segment {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Deref for Segment {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AsRef<str> for Segment {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for Segment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<&str> for Segment {
    type Error = ParseError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        validate_segment_str(value)?;
        Ok(Self(value.to_string()))
    }
}

impl TryFrom<String> for Segment {
    type Error = ParseError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        validate_segment_str(&value)?;
        Ok(Self(value))
    }
}

impl TryFrom<&OsStr> for Segment {
    type Error = ParseError;

    fn try_from(value: &OsStr) -> Result<Self, Self::Error> {
        let Some(value) = value.to_str() else {
            return Err(ParseError::NonUtf8Segment);
        };
        Self::try_from(value)
    }
}

impl Serialize for Segment {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Segment {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::try_from(value).map_err(serde::de::Error::custom)
    }
}

fn validate_segment_str(segment: &str) -> Result<(), ParseError> {
    if segment.is_empty() {
        return Err(ParseError::EmptySegment);
    }
    if segment.contains('/') {
        return Err(ParseError::SlashInSegment(segment.to_string()));
    }
    if matches!(segment, "." | "..") {
        return Err(ParseError::RelativeSegment(segment.to_string()));
    }
    // Reject control characters. Besides being invalid in real filenames, this
    // keeps low separator bytes (e.g. the cache's `\x1f` aux separator) free of
    // collisions with path content.
    if segment.chars().any(char::is_control) {
        return Err(ParseError::ControlCharInSegment(segment.to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod path_tests {
    use super::{ParseError, Path, Segment};

    #[test]
    fn parses_root_and_segments() {
        let path = Path::parse("/owner/repo").unwrap();
        assert_eq!(path.as_str(), "/owner/repo");
        assert_eq!(path.name(), "repo");
        assert_eq!(path.parent().unwrap().as_str(), "/owner");
        assert_eq!(path.segments().collect::<Vec<_>>(), vec!["owner", "repo"]);

        let root = Path::parse("/").unwrap();
        assert!(root.is_root());
        assert_eq!(root.name(), "");
        assert!(root.parent().is_none());
    }

    #[test]
    fn rejects_invalid_protocol_paths() {
        assert!(matches!(Path::parse(""), Err(ParseError::Empty)));
        assert!(matches!(
            Path::parse("owner/repo"),
            Err(ParseError::MissingLeadingSlash(_))
        ));
        assert!(matches!(
            Path::parse("/owner/repo/"),
            Err(ParseError::TrailingSlash(_))
        ));
        assert!(matches!(
            Path::parse("/owner//repo"),
            Err(ParseError::DoubleSlash(_))
        ));
        assert!(matches!(
            Path::parse("/owner/./repo"),
            Err(ParseError::RelativeSegment(_))
        ));
        assert!(matches!(
            Path::parse("/owner/../repo"),
            Err(ParseError::RelativeSegment(_))
        ));
        // Control characters (e.g. the cache's 0x1F separator) are rejected so
        // path content can never collide with a low separator byte.
        assert!(matches!(
            Path::parse("/owner/re\x1fpo"),
            Err(ParseError::ControlCharInSegment(_))
        ));
        assert!(matches!(
            Path::parse("/owner/re\u{0}po"),
            Err(ParseError::ControlCharInSegment(_))
        ));
    }

    #[test]
    fn validates_child_segments_before_joining() {
        let root = Path::root();
        let child = Segment::try_from("repo").unwrap();
        assert_eq!(root.join_segment(&child).as_str(), "/repo");
        assert_eq!(
            Path::parse("/owner")
                .unwrap()
                .join("repo")
                .unwrap()
                .as_str(),
            "/owner/repo"
        );

        assert!(matches!(
            Segment::try_from("nested/name"),
            Err(ParseError::SlashInSegment(_))
        ));
        assert!(matches!(
            Segment::try_from("."),
            Err(ParseError::RelativeSegment(_))
        ));
        assert!(matches!(
            Segment::try_from(".."),
            Err(ParseError::RelativeSegment(_))
        ));
    }

    #[test]
    fn prefix_operations_are_segment_boundary_safe() {
        let path = Path::parse("/foo/bar/baz").unwrap();
        assert!(path.has_prefix(&Path::parse("/foo/bar").unwrap()));
        assert!(
            !Path::parse("/foo/barbecue")
                .unwrap()
                .has_prefix(&Path::parse("/foo/bar").unwrap())
        );
        assert_eq!(
            path.strip_prefix(&Path::parse("/foo").unwrap()).unwrap(),
            Path::parse("/bar/baz").unwrap()
        );
        assert_eq!(
            path.strip_prefix(&Path::parse("/foo/bar/baz").unwrap())
                .unwrap(),
            Path::root()
        );
    }

    #[test]
    fn serde_uses_the_durable_string_shape() {
        let path = Path::parse("/owner/repo").unwrap();
        assert_eq!(serde_json::to_string(&path).unwrap(), "\"/owner/repo\"");
        assert_eq!(
            serde_json::from_str::<Path>("\"/owner/repo\"").unwrap(),
            path
        );
        assert!(serde_json::from_str::<Path>("\"/owner//repo\"").is_err());
    }
}
