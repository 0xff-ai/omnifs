use std::fmt;
use std::str::FromStr;
use thiserror::Error;

const MOUNT_NAME_HINT: &str =
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
    #[error("mount name must be 1-32 chars ({MOUNT_NAME_HINT})")]
    InvalidLength,
    #[error("mount name must start with a letter or digit ({MOUNT_NAME_HINT})")]
    InvalidStart,
    #[error("mount name contains invalid character `{ch}` ({MOUNT_NAME_HINT})")]
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
