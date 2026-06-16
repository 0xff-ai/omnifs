use omnifs_core::path::Segment;

/// Validated `NFSv4` component name.
///
/// Centralises the byte-level and path-safety checks for `LOOKUP`,
/// `OPEN CLAIM_NULL`, and export lookup. Rejects empty names, embedded
/// NUL, `/`, `\`, and relative path components. The export owns parent
/// traversal through `LOOKUPP`; component names must not encode path movement
/// themselves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentName(Segment);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InvalidName;

impl ComponentName {
    pub fn parse(name: &str) -> Result<Self, InvalidName> {
        if name.as_bytes().contains(&0) || name.contains('\\') {
            return Err(InvalidName);
        }
        Segment::try_from(name).map(Self).map_err(|_| InvalidName)
    }
}

impl AsRef<str> for ComponentName {
    fn as_ref(&self) -> &str {
        self.0.as_str()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_forbidden() {
        for name in [
            "",
            "a/b",
            "a\\b",
            "\0",
            "bad\0name",
            "/etc",
            "../escape",
            "a/../b",
            ".",
            "..",
        ] {
            assert!(
                ComponentName::parse(name).is_err(),
                "expected invalid: {name:?}"
            );
        }
    }
}
