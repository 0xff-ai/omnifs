use core::fmt;
use core::str::FromStr;

/// Validates the Docker name characters used for container, project,
/// and service names: must start with `[a-zA-Z0-9]` and continue with
/// `[a-zA-Z0-9_.-]+`. Mirrors the regex Docker enforces server-side
/// (`/?[a-zA-Z0-9][a-zA-Z0-9_.-]+`); the leading `/` Docker prefixes
/// to container names is stripped before this validator runs.
fn is_valid_docker_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    if !first.is_ascii_alphanumeric() {
        return false;
    }
    bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContainerName(String);

impl FromStr for ContainerName {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let trimmed = value.strip_prefix('/').unwrap_or(value);
        if is_valid_docker_name(trimmed) {
            Ok(Self(trimmed.to_string()))
        } else {
            Err(())
        }
    }
}

impl fmt::Display for ContainerName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContainerId(String);

impl FromStr for ContainerId {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        // Docker ids are 64 hex chars; clients commonly use the
        // 12-char short prefix for user-facing display. Accept the
        // full range from the short form up to the full hex digest.
        if (12..=64).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_hexdigit()) {
            Ok(Self(value.to_string()))
        } else {
            Err(())
        }
    }
}

impl fmt::Display for ContainerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProjectName(String);

impl ProjectName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for ProjectName {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if is_valid_docker_name(value) {
            Ok(Self(value.to_string()))
        } else {
            Err(())
        }
    }
}

impl fmt::Display for ProjectName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ServiceName(String);

impl ServiceName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for ServiceName {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if is_valid_docker_name(value) {
            Ok(Self(value.to_string()))
        } else {
            Err(())
        }
    }
}

impl fmt::Display for ServiceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
