use std::fmt;
use std::str::FromStr;

use cid::Cid;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct CidText {
    canonical: String,
    codec: u64,
}

impl CidText {
    pub fn codec(&self) -> u64 {
        self.codec
    }
}

#[derive(Debug)]
pub struct CidParseError(String);

impl fmt::Display for CidParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid CID: {}", self.0)
    }
}

impl std::error::Error for CidParseError {}

impl FromStr for CidText {
    type Err = CidParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let cid = Cid::try_from(s).map_err(|e| CidParseError(e.to_string()))?;
        // Canonicalize to CIDv1 base32-lower so users see the same form
        // regardless of how they entered it. CIDv0 inputs round-trip to
        // their CIDv1 equivalent (codec = dag-pb, base = base32-lower).
        let canonical_cid = cid.into_v1().map_err(|e| CidParseError(e.to_string()))?;
        Ok(Self {
            canonical: canonical_cid.to_string(),
            codec: canonical_cid.codec(),
        })
    }
}

impl fmt::Display for CidText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.canonical)
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct IpnsName(String);

#[derive(Debug)]
pub struct IpnsNameParseError(&'static str);

impl fmt::Display for IpnsNameParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for IpnsNameParseError {}

impl FromStr for IpnsName {
    type Err = IpnsNameParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err(IpnsNameParseError("empty IPNS name"));
        }
        if s.contains('/') || s.chars().any(char::is_whitespace) {
            return Err(IpnsNameParseError(
                "IPNS name must not contain '/' or whitespace",
            ));
        }
        Ok(Self(s.to_string()))
    }
}

impl fmt::Display for IpnsName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cidtext_canonicalizes_v0_to_v1() {
        let v0 = "QmYwAPJzv5CZsnA625s3Xf2nemtYgPpHdWEz79ojWnPbdG";
        let parsed: CidText = v0.parse().unwrap();
        assert!(parsed.to_string().starts_with("bafy"));
    }

    #[test]
    fn ipns_name_rejects_path_separator() {
        assert!("foo/bar".parse::<IpnsName>().is_err());
    }

    #[test]
    fn ipns_name_rejects_whitespace() {
        assert!("foo bar".parse::<IpnsName>().is_err());
    }

    #[test]
    fn ipns_name_accepts_dnslink() {
        assert!("docs.ipfs.tech".parse::<IpnsName>().is_ok());
    }
}
