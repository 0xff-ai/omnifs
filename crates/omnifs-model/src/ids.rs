use std::error::Error;
use std::fmt;
use std::str::FromStr;

const MOUNT_NAME_HINT: &str =
    "lowercase letters, digits, dashes; 1-32 chars; start with a letter/digit";
const KEY_PART_HINT: &str = "letters, digits, dashes, underscores, or dots; 1-128 chars";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MountName(String);

impl MountName {
    pub fn new(name: impl Into<String>) -> Result<Self, MountNameError> {
        let name = name.into();
        validate_mount_name(&name)?;
        Ok(Self(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MountName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for MountName {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl FromStr for MountName {
    type Err = MountNameError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl TryFrom<String> for MountName {
    type Error = MountNameError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<&str> for MountName {
    type Error = MountNameError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountNameError {
    message: String,
}

impl fmt::Display for MountNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for MountNameError {}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProviderId(String);

impl ProviderId {
    pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
        let value = value.into();
        validate_key_part("provider_id", &value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for ProviderId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl FromStr for ProviderId {
    type Err = IdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AuthSchemeId(String);

impl AuthSchemeId {
    pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
        let value = value.into();
        validate_key_part("scheme", &value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AuthSchemeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for AuthSchemeId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl FromStr for AuthSchemeId {
    type Err = IdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AccountId(String);

impl AccountId {
    pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
        let value = value.into();
        validate_account(&value)?;
        Ok(Self(value))
    }

    pub fn default_account() -> Self {
        Self("default".to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for AccountId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl FromStr for AccountId {
    type Err = IdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdError {
    message: String,
}

impl fmt::Display for IdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for IdError {}

pub(crate) fn validate_mount_name(name: &str) -> Result<(), MountNameError> {
    if name.is_empty() || name.len() > 32 {
        return Err(MountNameError {
            message: format!("mount name must be 1-32 chars ({MOUNT_NAME_HINT})"),
        });
    }
    let mut chars = name.chars();
    let first = chars.next().expect("non-empty checked above");
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return Err(MountNameError {
            message: format!("mount name must start with a letter or digit ({MOUNT_NAME_HINT})"),
        });
    }
    for ch in chars {
        if !(ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-') {
            return Err(MountNameError {
                message: format!(
                    "mount name contains invalid character `{ch}` ({MOUNT_NAME_HINT})"
                ),
            });
        }
    }
    Ok(())
}

pub(crate) fn validate_key_part(field: &'static str, value: &str) -> Result<(), IdError> {
    if value.is_empty() {
        return Err(IdError {
            message: format!("{field} cannot be empty ({KEY_PART_HINT})"),
        });
    }
    if value.len() > 128 {
        return Err(IdError {
            message: format!("{field} is too long: {} bytes, max 128", value.len()),
        });
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(IdError {
            message: format!("invalid {field} `{value}` ({KEY_PART_HINT})"),
        });
    }
    Ok(())
}

pub(crate) fn validate_account(value: &str) -> Result<(), IdError> {
    if value.is_empty() {
        return Err(IdError {
            message: "account cannot be empty".to_owned(),
        });
    }
    if value.len() > 128 {
        return Err(IdError {
            message: format!("account is too long: {} bytes, max 128", value.len()),
        });
    }
    if value
        .chars()
        .any(|c| c.is_control() || matches!(c, '/' | '\\'))
    {
        return Err(IdError {
            message: format!("invalid account `{value}`"),
        });
    }
    Ok(())
}
