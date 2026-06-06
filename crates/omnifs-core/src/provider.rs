use std::fmt;
use std::str::FromStr;
use thiserror::Error;

const KEY_PART_HINT: &str = "letters, digits, dashes, underscores, or dots; 1-128 chars";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Id(String);

impl Id {
    pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
        let value = value.into();
        validate_key_part("provider_id", &value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for Id {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl FromStr for Id {
    type Err = IdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum IdError {
    #[error("{field} cannot be empty ({KEY_PART_HINT})")]
    Empty { field: &'static str },
    #[error("{field} is too long: {len} bytes, max 128")]
    TooLong { field: &'static str, len: usize },
    #[error("invalid {field} `{value}` ({KEY_PART_HINT})")]
    Invalid { field: &'static str, value: String },
    #[error("account cannot be empty")]
    AccountEmpty,
    #[error("account is too long: {len} bytes, max 128")]
    AccountTooLong { len: usize },
    #[error("invalid account `{value}`")]
    InvalidAccount { value: String },
}

pub(crate) fn validate_key_part(field: &'static str, value: &str) -> Result<(), IdError> {
    if value.is_empty() {
        return Err(IdError::Empty { field });
    }
    if value.len() > 128 {
        return Err(IdError::TooLong {
            field,
            len: value.len(),
        });
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(IdError::Invalid {
            field,
            value: value.to_owned(),
        });
    }
    Ok(())
}

pub(crate) fn validate_account(value: &str) -> Result<(), IdError> {
    if value.is_empty() {
        return Err(IdError::AccountEmpty);
    }
    if value.len() > 128 {
        return Err(IdError::AccountTooLong { len: value.len() });
    }
    if value
        .chars()
        .any(|c| c.is_control() || matches!(c, '/' | '\\'))
    {
        return Err(IdError::InvalidAccount {
            value: value.to_owned(),
        });
    }
    Ok(())
}
