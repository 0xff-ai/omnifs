use std::path::Path;

/// Validated `NFSv4` component name.
///
/// Centralises the byte-level and path-safety checks for `LOOKUP`,
/// `OPEN CLAIM_NULL`, and export lookup. Rejects empty names, embedded
/// NUL, `/`, `\`, and names that resolve to non-Normal path components,
/// including `.` and `..`. The export owns parent traversal through
/// `LOOKUPP`; component names must not encode path movement themselves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentName(String);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InvalidName;

impl ComponentName {
    pub fn parse(name: &str) -> Result<Self, InvalidName> {
        if name.is_empty()
            || name.as_bytes().contains(&0)
            || name.contains('/')
            || name.contains('\\')
        {
            return Err(InvalidName);
        }

        let mut components = Path::new(name).components();
        if !matches!(components.next(), Some(std::path::Component::Normal(_)))
            || components.next().is_some()
        {
            return Err(InvalidName);
        }

        Ok(Self(name.to_string()))
    }
}

impl AsRef<str> for ComponentName {
    fn as_ref(&self) -> &str {
        &self.0
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
