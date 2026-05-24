use anyhow::{Result, bail};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub prerelease: Option<String>,
}

pub fn parse_version(input: &str) -> Result<Version> {
    let (core, prerelease) = match input.split_once('-') {
        Some((core, pre)) => (core, Some(pre.to_string())),
        None => (input, None),
    };
    let mut parts = core.split('.');
    let major = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("invalid version: {input}"))?
        .parse()?;
    let minor = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("invalid version: {input}"))?
        .parse()?;
    let patch = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("invalid version: {input}"))?
        .parse()?;
    if parts.next().is_some() {
        bail!("invalid version: {input}");
    }
    Ok(Version {
        major,
        minor,
        patch,
        prerelease,
    })
}

pub fn bump_patch(version: &str) -> Result<String> {
    let v = parse_version(version)?;
    Ok(format!("{}.{}.{}", v.major, v.minor, v.patch + 1))
}
