use std::net::IpAddr;
use std::str::FromStr;

use hickory_proto::rr::RecordType as HickoryRecordType;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct SupportedRecordType(HickoryRecordType);

impl SupportedRecordType {
    const SUPPORTED: &'static [Self] = &[
        Self::A,
        Self::AAAA,
        Self::CNAME,
        Self::MX,
        Self::NS,
        Self::TXT,
        Self::SOA,
        Self::SRV,
        Self::CAA,
        Self::PTR,
    ];

    pub const A: Self = Self(HickoryRecordType::A);
    pub const AAAA: Self = Self(HickoryRecordType::AAAA);
    pub const CNAME: Self = Self(HickoryRecordType::CNAME);
    pub const MX: Self = Self(HickoryRecordType::MX);
    pub const NS: Self = Self(HickoryRecordType::NS);
    pub const TXT: Self = Self(HickoryRecordType::TXT);
    pub const SOA: Self = Self(HickoryRecordType::SOA);
    pub const SRV: Self = Self(HickoryRecordType::SRV);
    pub const CAA: Self = Self(HickoryRecordType::CAA);
    pub const PTR: Self = Self(HickoryRecordType::PTR);

    /// PTR excluded: it is only used internally for `_reverse/<ip>`.
    pub fn all() -> &'static [Self] {
        &[
            Self::A,
            Self::AAAA,
            Self::CNAME,
            Self::MX,
            Self::NS,
            Self::TXT,
            Self::SOA,
            Self::SRV,
            Self::CAA,
        ]
    }

    /// Subset queried in parallel for `_all` (skip SRV/CAA to reduce noise).
    pub fn common() -> &'static [Self] {
        &[
            Self::A,
            Self::AAAA,
            Self::CNAME,
            Self::MX,
            Self::NS,
            Self::TXT,
            Self::SOA,
        ]
    }

    pub fn from_hickory(rtype: HickoryRecordType) -> Option<Self> {
        Self::SUPPORTED
            .iter()
            .copied()
            .find(|supported| supported.0 == rtype)
    }

    pub fn as_hickory(self) -> HickoryRecordType {
        self.0
    }

    pub fn as_str(self) -> &'static str {
        self.0.into()
    }
}

impl FromStr for SupportedRecordType {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value
            .parse::<HickoryRecordType>()
            .ok()
            .and_then(Self::from_hickory)
            .ok_or(())
    }
}

impl std::fmt::Display for SupportedRecordType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl AsRef<str> for SupportedRecordType {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct DomainName(String);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ResolverName(String);

impl FromStr for DomainName {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        (s.parse::<IpAddr>().is_err()
            && s.contains('.')
            && !s.contains(char::is_whitespace)
            && s.len() <= 253)
            .then_some(Self(s.to_string()))
            .ok_or(())
    }
}

impl std::fmt::Display for DomainName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl AsRef<str> for DomainName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl FromStr for ResolverName {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        (!s.is_empty() && !s.contains('/') && !s.contains(char::is_whitespace))
            .then_some(Self(s.to_string()))
            .ok_or(())
    }
}

impl std::fmt::Display for ResolverName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl AsRef<str> for ResolverName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}
