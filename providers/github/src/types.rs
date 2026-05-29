//! Domain types for the GitHub provider's virtual filesystem structure.

use core::str::FromStr;
use serde::Deserialize;

/// State filter for resources.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, strum::EnumString, strum::AsRefStr)]
pub enum StateFilter {
    #[strum(serialize = "open")]
    Open,
    #[strum(serialize = "all")]
    All,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OwnerName(String);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepoName(String);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct RepoId {
    owner: OwnerName,
    repo: RepoName,
}

impl RepoId {
    pub(crate) fn new(owner: &OwnerName, repo: &RepoName) -> Self {
        Self {
            owner: owner.clone(),
            repo: repo.clone(),
        }
    }

    pub(crate) fn parse(path: &str) -> Option<Self> {
        let mut segments = path.trim_start_matches('/').split('/');
        let owner = segments.next()?.parse().ok()?;
        let repo = segments.next()?.parse().ok()?;
        (segments.next().is_none()).then_some(Self { owner, repo })
    }
}

impl std::fmt::Display for RepoId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.owner, self.repo)
    }
}

impl FromStr for OwnerName {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        is_safe_segment(s).then_some(Self(s.to_string())).ok_or(())
    }
}

impl FromStr for RepoName {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        is_safe_segment(s).then_some(Self(s.to_string())).ok_or(())
    }
}

impl AsRef<str> for OwnerName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for OwnerName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl AsRef<str> for RepoName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RepoName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct User {
    pub(crate) login: String,
}

/// Validates that a path segment is a safe GitHub owner, repo, or
/// numeric ID. GitHub permits leading dots in repo names (`.github`
/// is the canonical per-org community-config repo), so we only block
/// the two path-traversal cases.
pub fn is_safe_segment(s: &str) -> bool {
    if s.is_empty() || s == "." || s == ".." {
        return false;
    }
    s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_leading_dot_repo_names() {
        // `.github` is the canonical per-org community-config repo and
        // appears in repo listings for most active orgs. Rejecting it
        // (as the previous validator did) breaks the whole org listing
        // since one bad name aborts the `collect::<Result<_>>()` pass.
        assert!(is_safe_segment(".github"));
        assert!(is_safe_segment(".gitignore"));
    }

    #[test]
    fn rejects_path_traversal_names() {
        assert!(!is_safe_segment("."));
        assert!(!is_safe_segment(".."));
    }

    #[test]
    fn rejects_empty() {
        assert!(!is_safe_segment(""));
    }

    #[test]
    fn rejects_unsafe_chars() {
        assert!(!is_safe_segment("foo/bar"));
        assert!(!is_safe_segment("foo bar"));
        assert!(!is_safe_segment("foo\nbar"));
        assert!(!is_safe_segment("foo:bar"));
    }

    #[test]
    fn accepts_typical_names() {
        assert!(is_safe_segment("omnifs"));
        assert!(is_safe_segment("0xff-ai"));
        assert!(is_safe_segment("test_repo"));
        assert!(is_safe_segment("rust-1.85"));
    }
}
