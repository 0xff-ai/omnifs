//! Domain types for the Linear provider's virtual filesystem structure.

use core::str::FromStr;

/// State filter directories under `/teams/{KEY}/issues/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, strum::EnumString, strum::AsRefStr)]
pub enum StateFilter {
    /// Open issues. Linear state types in `{triage, backlog, unstarted, started}`.
    #[strum(serialize = "_open")]
    Open,
    /// All issues regardless of state.
    #[strum(serialize = "_all")]
    All,
}

/// A Linear team key (e.g. `ENG`, `OPS`). Uppercase ASCII alphanumeric.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TeamKey(String);

impl TeamKey {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for TeamKey {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() || s.len() > 32 {
            return Err(());
        }
        // Linear team keys are uppercase ASCII alphanumeric. Accept
        // hyphen/underscore too so we don't fight workspaces that use
        // them; reject anything that wouldn't make a safe path segment.
        let ok = s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if !ok {
            return Err(());
        }
        Ok(Self(s.to_string()))
    }
}

impl AsRef<str> for TeamKey {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TeamKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// A Linear issue identifier (e.g. `ENG-1234`). The textual form is
/// what users type and what Linear's API accepts in `Issue.identifier`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IssueIdent {
    team: TeamKey,
    number: u64,
}

impl IssueIdent {
    pub fn team(&self) -> &TeamKey {
        &self.team
    }
}

impl FromStr for IssueIdent {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (team, number) = s.rsplit_once('-').ok_or(())?;
        let team = team.parse::<TeamKey>()?;
        let number = number.parse::<u64>().map_err(|_| ())?;
        if number == 0 {
            return Err(());
        }
        Ok(Self { team, number })
    }
}

impl std::fmt::Display for IssueIdent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}-{}", self.team, self.number)
    }
}

/// Linear workflow state type. Linear groups states under one of these
/// types; the `_open` filter selects everything that is not `completed`
/// or `canceled`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StateType {
    Triage,
    Backlog,
    Unstarted,
    Started,
    Completed,
    Canceled,
}
