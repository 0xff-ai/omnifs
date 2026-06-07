//! Pre-OAuth context detection. Probes env vars and the `gh` CLI for
//! credentials the user likely already has. The init flow can offer to
//! import these instead of starting OAuth.

use secrecy::SecretString;

#[derive(Debug, Clone)]
pub enum DetectedCredential {
    /// An environment variable carries a token.
    EnvVar { name: String, value: SecretString },
    /// `gh auth token` returned a logged-in token. Scope/account metadata
    /// comes from `gh auth status` when available.
    GhCli {
        account: String,
        scopes: Vec<String>,
        token: SecretString,
    },
}

pub fn detect(provider_id: &str) -> Vec<DetectedCredential> {
    let mut found = Vec::new();
    match provider_id {
        "github" => {
            if let Some(value) = std::env::var_os("GITHUB_TOKEN")
                && !value.is_empty()
            {
                found.push(DetectedCredential::EnvVar {
                    name: "GITHUB_TOKEN".to_string(),
                    value: SecretString::from(value.to_string_lossy().into_owned()),
                });
            }
            if let Some(gh) = detect_gh_cli() {
                found.push(gh);
            }
        },
        "linear" => {
            if let Some(value) = std::env::var_os("LINEAR_API_KEY")
                && !value.is_empty()
            {
                found.push(DetectedCredential::EnvVar {
                    name: "LINEAR_API_KEY".to_string(),
                    value: SecretString::from(value.to_string_lossy().into_owned()),
                });
            }
        },
        _ => {},
    }
    found
}

fn detect_gh_cli() -> Option<DetectedCredential> {
    let token = gh_auth_token()?;
    let (account, scopes) =
        gh_auth_identity().unwrap_or_else(|| ("unknown".to_string(), Vec::new()));
    Some(DetectedCredential::GhCli {
        account,
        scopes,
        token: SecretString::from(token),
    })
}

fn gh_auth_token() -> Option<String> {
    let output = std::process::Command::new("gh")
        .args(["auth", "token"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!token.is_empty()).then_some(token)
}

fn gh_auth_identity() -> Option<(String, Vec<String>)> {
    let output = std::process::Command::new("gh")
        .args(["auth", "status"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let account = combined.lines().find_map(|line| {
        line.split_once("account ").map(|(_, rest)| {
            rest.split_whitespace()
                .next()
                .unwrap_or("")
                .trim_start_matches('@')
                .to_string()
        })
    })?;

    let scopes = combined
        .lines()
        .find_map(|line| {
            line.split_once("Token scopes:").map(|(_, rest)| {
                rest.split(',')
                    .map(|scope| scope.trim().trim_matches('\'').to_string())
                    .filter(|scope| !scope.is_empty())
                    .collect()
            })
        })
        .unwrap_or_default();

    Some((account, scopes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(unsafe_code)]
    fn detect_github_env_var() {
        // SAFETY: env mutation is isolated to the duration of this test.
        // This test must not run concurrently with other tests that set GITHUB_TOKEN.
        unsafe {
            std::env::set_var("GITHUB_TOKEN", "ghp_abc");
        }
        let found = detect("github");
        let has_env_var = found.iter().any(
            |d| matches!(d, DetectedCredential::EnvVar { name, .. } if name == "GITHUB_TOKEN"),
        );
        assert!(has_env_var, "found: {found:?}");
        unsafe {
            std::env::remove_var("GITHUB_TOKEN");
        }
    }
}
