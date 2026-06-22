//! The required-capabilities invariant: a mount's [`Grants`] must satisfy every
//! capability its provider's manifest declares it [`Need`]s. The host runs this
//! at provider start so an under-granted mount fails fast, rather than at the
//! provider's first denied callout.

use crate::matching::glob_covers;
use crate::model::{DynamicMarker, Grant, Grants, Missing, Need, PreopenedPath};

impl Grants {
    /// The capabilities `needs` declares that these grants do not satisfy. An
    /// empty result means the grants cover the manifest.
    ///
    /// Semantics per kind:
    /// - A **dynamic** need is satisfied only by a dynamic grant of that kind
    ///   (the value is resolved from the mount endpoint at init, never listed).
    /// - A **domain** / **unix socket** need is satisfied by a literal grant
    ///   containing the value exactly (or `*` for domains).
    /// - A **git repo** need is satisfied by any literal grant within the
    ///   declared pattern's reach or that covers it: narrowing `git@github.com:*`
    ///   to a concrete repo is a valid tightening, not an under-grant.
    /// - A **preopen** need is satisfied by a literal grant with the same guest
    ///   path whose mode covers the needed mode; the host path is the operator's
    ///   choice and is not matched.
    /// - Scalar resource limits (memory, blob bytes) are not part of the check:
    ///   they carry host defaults and fail as resource exhaustion, not access
    ///   denial.
    #[must_use]
    pub fn satisfies(&self, needs: &[Need]) -> Vec<Missing> {
        needs.iter().filter_map(|need| self.unmet(need)).collect()
    }

    fn unmet(&self, need: &Need) -> Option<Missing> {
        match need {
            Need::Domain { value, dynamic, .. } => unmet_value(
                "domain",
                self.domains.as_ref(),
                value,
                *dynamic,
                Match::Exact,
            ),
            Need::GitRepo { value, dynamic, .. } => unmet_value(
                "gitRepo",
                self.git_repos.as_ref(),
                value,
                *dynamic,
                Match::Glob,
            ),
            Need::UnixSocket { value, dynamic, .. } => unmet_value(
                "unixSocket",
                self.unix_sockets.as_ref(),
                value,
                *dynamic,
                Match::Exact,
            ),
            Need::PreopenedPath { value, dynamic, .. } => {
                unmet_preopen(self.preopened_paths.as_ref(), value, *dynamic)
            },
            Need::MemoryMb { .. } | Need::FetchBlobBytes { .. } | Need::ReadBlobBytes { .. } => {
                None
            },
        }
    }

    /// Seed an explicit grant set from a provider's declared `needs`, used by
    /// `omnifs init`. Literal needs become literal grants; dynamic needs become
    /// the dynamic marker. This is a creation-time translation the resulting
    /// spec then owns; the manifest is never consulted to grant at runtime.
    #[must_use]
    pub fn from_needs(needs: &[Need]) -> Self {
        let mut grants = Self::default();
        for need in needs {
            match need {
                Need::Domain { value, dynamic, .. } => {
                    push(&mut grants.domains, value.clone(), *dynamic);
                },
                Need::GitRepo { value, dynamic, .. } => {
                    push(&mut grants.git_repos, value.clone(), *dynamic);
                },
                Need::UnixSocket { value, dynamic, .. } => {
                    push(&mut grants.unix_sockets, value.clone(), *dynamic);
                },
                Need::PreopenedPath { value, dynamic, .. } => {
                    push(&mut grants.preopened_paths, value.clone(), *dynamic);
                },
                Need::MemoryMb { value, .. } => grants.max_memory_mb = Some(*value),
                Need::FetchBlobBytes { value, .. } => grants.max_fetch_blob_bytes = Some(*value),
                Need::ReadBlobBytes { value, .. } => grants.max_read_blob_bytes = Some(*value),
            }
        }
        grants
    }
}

#[derive(Clone, Copy)]
enum Match {
    Exact,
    Glob,
}

impl Match {
    fn satisfied_by(self, grant: &str, need: &str) -> bool {
        match self {
            Match::Exact => grant == need || grant == "*",
            // Either the grant covers the need (over-grant) or the need's
            // pattern covers the grant (a narrower, valid tightening).
            Match::Glob => glob_covers(grant, need) || glob_covers(need, grant),
        }
    }
}

fn unmet_value(
    kind: &'static str,
    grant: Option<&Grant<String>>,
    need: &str,
    dynamic: bool,
    matcher: Match,
) -> Option<Missing> {
    match grant {
        Some(Grant::Dynamic(_)) if dynamic => None,
        Some(Grant::Literal(values))
            if !dynamic && values.iter().any(|g| matcher.satisfied_by(g, need)) =>
        {
            None
        },
        Some(Grant::Literal(_)) if dynamic => Some(Missing {
            kind,
            value: format!("{need} (dynamic)"),
        }),
        _ => Some(Missing {
            kind,
            value: need.to_string(),
        }),
    }
}

fn unmet_preopen(
    grant: Option<&Grant<PreopenedPath>>,
    need: &PreopenedPath,
    dynamic: bool,
) -> Option<Missing> {
    // Match on the guest path and mode, not the host: the guest path is the
    // contract the provider sees, while the host path is the operator's choice
    // of what to expose there (and is often config-derived, e.g. a database
    // file's directory).
    let satisfied = match grant {
        Some(Grant::Dynamic(_)) => dynamic,
        Some(Grant::Literal(grants)) if !dynamic => grants
            .iter()
            .any(|g| g.guest == need.guest && g.mode.covers(need.mode)),
        _ => false,
    };
    if satisfied {
        None
    } else {
        Some(Missing {
            kind: "preopenedPath",
            value: format!("{} -> {}", need.host, need.guest),
        })
    }
}

fn push<T>(field: &mut Option<Grant<T>>, value: T, dynamic: bool) {
    if dynamic {
        *field = Some(Grant::Dynamic(DynamicMarker::new()));
    } else if let Some(Grant::Literal(values)) = field {
        values.push(value);
    } else {
        *field = Some(Grant::Literal(vec![value]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PreopenMode;

    fn need_domain(value: &str) -> Need {
        Need::Domain {
            value: value.into(),
            why: "test".into(),
            dynamic: false,
        }
    }

    #[test]
    fn grants_seeded_from_needs_satisfy_them() {
        let needs = vec![
            need_domain("api.github.com"),
            Need::GitRepo {
                value: "git@github.com:*".into(),
                why: "clone".into(),
                dynamic: false,
            },
        ];
        let grants = Grants::from_needs(&needs);
        assert!(grants.satisfies(&needs).is_empty());
    }

    #[test]
    fn under_granted_domain_is_missing() {
        let grants = Grants::from_needs(&[need_domain("api.github.com")]);
        let needs = vec![
            need_domain("api.github.com"),
            need_domain("uploads.github.com"),
        ];
        let missing = grants.satisfies(&needs);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].value, "uploads.github.com");
    }

    #[test]
    fn narrowing_a_glob_repo_grant_still_satisfies() {
        let need = Need::GitRepo {
            value: "git@github.com:*".into(),
            why: "clone".into(),
            dynamic: false,
        };
        let grants = Grants {
            git_repos: Some(Grant::Literal(vec!["git@github.com:me/repo".into()])),
            ..Grants::default()
        };
        assert!(grants.satisfies(std::slice::from_ref(&need)).is_empty());
    }

    #[test]
    fn unrelated_repo_grant_does_not_satisfy() {
        let need = Need::GitRepo {
            value: "git@github.com:*".into(),
            why: "clone".into(),
            dynamic: false,
        };
        let grants = Grants {
            git_repos: Some(Grant::Literal(vec!["git@gitlab.com:me/repo".into()])),
            ..Grants::default()
        };
        assert_eq!(grants.satisfies(std::slice::from_ref(&need)).len(), 1);
    }

    #[test]
    fn dynamic_need_requires_a_dynamic_grant() {
        let need = Need::UnixSocket {
            value: "configured Docker socket".into(),
            why: "talk to docker".into(),
            dynamic: true,
        };
        // A literal grant does not satisfy a dynamic need.
        let literal = Grants {
            unix_sockets: Some(Grant::Literal(vec!["/var/run/docker.sock".into()])),
            ..Grants::default()
        };
        assert_eq!(literal.satisfies(std::slice::from_ref(&need)).len(), 1);

        // The dynamic marker does (resolved from the endpoint at init).
        let dynamic = Grants::from_needs(std::slice::from_ref(&need));
        assert!(dynamic.unix_sockets.as_ref().unwrap().is_dynamic());
        assert!(dynamic.satisfies(std::slice::from_ref(&need)).is_empty());
    }

    #[test]
    fn preopen_mode_must_cover_the_need() {
        let need = Need::PreopenedPath {
            value: PreopenedPath {
                host: "/data".into(),
                guest: "/data".into(),
                mode: PreopenMode::Rw,
            },
            why: "write".into(),
            dynamic: false,
        };
        let ro = Grants {
            preopened_paths: Some(Grant::Literal(vec![PreopenedPath {
                host: "/data".into(),
                guest: "/data".into(),
                mode: PreopenMode::Ro,
            }])),
            ..Grants::default()
        };
        assert_eq!(ro.satisfies(std::slice::from_ref(&need)).len(), 1);
    }
}
