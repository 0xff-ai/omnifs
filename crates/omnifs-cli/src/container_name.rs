use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ContainerName(String);

impl ContainerName {
    pub(crate) fn new(name: impl Into<String>) -> anyhow::Result<Self> {
        let name = name.into();
        validate(&name)?;
        Ok(Self(name))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ContainerName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for ContainerName {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

/// Reject container names that could escape the temp session root once
/// interpolated into `omnifs-session-<name>`. Docker's own naming rules
/// already forbid path separators, but the host CLI validates before session
/// paths are created or removed.
fn validate(name: &str) -> anyhow::Result<()> {
    if name.is_empty() || name.len() > 64 {
        anyhow::bail!("container name must be 1-64 chars");
    }
    let first = name.chars().next().unwrap();
    if !(first.is_ascii_alphanumeric()) {
        anyhow::bail!("container name must start with a letter or digit (got `{first}`)");
    }
    for ch in name.chars() {
        if !(ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-')) {
            anyhow::bail!(
                "container name contains invalid character `{ch}`; allowed: letters, digits, _ . -"
            );
        }
    }
    Ok(())
}
