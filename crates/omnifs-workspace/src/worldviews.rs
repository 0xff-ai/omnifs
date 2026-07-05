//! Named read-only serving scopes over the configured mount namespace.
//!
//! A worldview lives at `<config_dir>/worldviews/<name>.json`. Loading is
//! strict: unknown fields are rejected, the file stem owns the worldview
//! identity, and v0 only accepts explicit `read_only: true` entries.

use std::fmt;
use std::path::{Path as StdPath, PathBuf};
use std::str::FromStr;

use omnifs_core::path::Path;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

const WORLDVIEW_NAME_HINT: &str =
    "lowercase letters, digits, dashes; 1-32 chars; start with a letter/digit";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Name(String);

impl Name {
    pub fn new(name: impl Into<String>) -> Result<Self, NameError> {
        let name = name.into();
        validate_name(&name)?;
        Ok(Self(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for Name {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl FromStr for Name {
    type Err = NameError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl TryFrom<String> for Name {
    type Error = NameError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<&str> for Name {
    type Error = NameError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum NameError {
    #[error("worldview name must be 1-32 chars ({WORLDVIEW_NAME_HINT})")]
    InvalidLength,
    #[error("worldview name must start with a letter or digit ({WORLDVIEW_NAME_HINT})")]
    InvalidStart,
    #[error("worldview name contains invalid character `{ch}` ({WORLDVIEW_NAME_HINT})")]
    InvalidCharacter { ch: char },
}

fn validate_name(name: &str) -> Result<(), NameError> {
    if name.is_empty() || name.len() > 32 {
        return Err(NameError::InvalidLength);
    }
    let mut chars = name.chars();
    let first = chars.next().expect("non-empty checked above");
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return Err(NameError::InvalidStart);
    }
    for ch in chars {
        if !(ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-') {
            return Err(NameError::InvalidCharacter { ch });
        }
    }
    Ok(())
}

/// Marker for the v0 invariant that every worldview mount is read-only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReadOnly;

impl Serialize for ReadOnly {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_bool(true)
    }
}

impl<'de> Deserialize<'de> for ReadOnly {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = bool::deserialize(deserializer)?;
        if value {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom(
                "read_only must be true in worldview v0",
            ))
        }
    }
}

/// A named, strict-parsed serving scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Worldview {
    pub name: String,
    pub mounts: Vec<Mount>,
}

impl Worldview {
    pub fn parse(content: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(content)
    }

    /// Load `<worldviews_dir>/<name>.json`, requiring the inner `name` to match
    /// the file stem requested by the caller.
    pub fn load(worldviews_dir: impl AsRef<StdPath>, name: &str) -> Result<Self, Error> {
        let name = Name::new(name).map_err(|source| Error::Name {
            name: name.to_string(),
            source,
        })?;
        let path = worldviews_dir.as_ref().join(format!("{name}.json"));
        let content = std::fs::read_to_string(&path).map_err(|source| Error::Read {
            path: path.clone(),
            source,
        })?;
        let worldview = Self::parse(&content).map_err(|source| Error::Parse {
            path: path.clone(),
            source,
        })?;
        if worldview.name != name.as_str() {
            return Err(Error::FilenameMismatch {
                path,
                file_name: name.to_string(),
                declared: worldview.name,
            });
        }
        Ok(worldview)
    }
}

/// One mount admitted into a worldview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mount {
    pub mount: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtree: Option<Path>,
    pub read_only: ReadOnly,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid worldview name `{name}`: {source}")]
    Name { name: String, source: NameError },
    #[error("failed to read worldview {}: {source}", path.display())]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse worldview {}: {source}", path.display())]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error(
        "worldview file {} declares name `{declared}` but must be named `{file_name}.json`",
        path.display()
    )]
    FilenameMismatch {
        path: PathBuf,
        file_name: String,
        declared: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rejects_unknown_keys_at_every_level() {
        let top = serde_json::from_value::<Worldview>(serde_json::json!({
            "name": "dev",
            "mounts": [],
            "moutns": []
        }))
        .expect_err("unknown top-level key must fail");
        assert!(
            top.to_string().contains("unknown field `moutns`"),
            "error should name typo: {top}"
        );

        let nested = serde_json::from_value::<Worldview>(serde_json::json!({
            "name": "dev",
            "mounts": [
                { "mount": "github", "read_only": true, "subtre": "/repo" }
            ]
        }))
        .expect_err("unknown mount key must fail");
        assert!(
            nested.to_string().contains("unknown field `subtre`"),
            "error should name nested typo: {nested}"
        );
    }

    #[test]
    fn parse_rejects_read_only_false() {
        let error = serde_json::from_value::<Worldview>(serde_json::json!({
            "name": "dev",
            "mounts": [
                { "mount": "github", "read_only": false }
            ]
        }))
        .expect_err("worldview v0 must reject writable entries");

        let message = error.to_string();
        assert!(
            message.contains("read_only must be true"),
            "error should name read_only: {message}"
        );
    }

    #[test]
    fn load_rejects_name_mismatch() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            dir.path().join("dev.json"),
            r#"{ "name": "prod", "mounts": [] }"#,
        )
        .unwrap();

        let error = Worldview::load(dir.path(), "dev").expect_err("name mismatch must fail");

        assert!(matches!(error, Error::FilenameMismatch { .. }));
    }

    #[test]
    fn parse_rejects_bad_subtree_path() {
        let error = serde_json::from_value::<Worldview>(serde_json::json!({
            "name": "dev",
            "mounts": [
                { "mount": "github", "subtree": "relative", "read_only": true }
            ]
        }))
        .expect_err("subtree must be an absolute omnifs path");

        let message = error.to_string();
        assert!(
            message.contains("path is not absolute"),
            "error should preserve path parse cause: {message}"
        );
    }
}
