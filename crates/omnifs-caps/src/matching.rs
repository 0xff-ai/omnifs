//! Matching primitives shared by the resolved [`Allowlist`](crate::Allowlist)
//! the host enforces and the start-time `satisfies` check. A single definition
//! is the point of the shared crate: enforcement and the check can never drift
//! to two different notions of "allowed".

/// Whether `pattern` covers `value`. A trailing `*` is a prefix wildcard
/// (`git@github.com:*` covers `git@github.com:me/repo`); otherwise the match is
/// exact. This is the git-repo rule the host enforces on every git callout.
#[must_use]
pub fn glob_covers(pattern: &str, value: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => value.starts_with(prefix),
        None => value == pattern,
    }
}

/// Whether an allowlisted domain entry matches a callout host. `*` is the
/// match-all wildcard; otherwise the host must equal the entry exactly.
#[must_use]
pub fn domain_matches(allowed: &str, host: &str) -> bool {
    allowed == "*" || allowed == host
}
