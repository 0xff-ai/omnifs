use super::detect;
use crate::auth::AuthSelection;
use omnifs_provider::AuthManifest;
use secrecy::SecretString;

pub(crate) struct ImportOutcome {
    pub(crate) auth: Option<AuthSelection>,
    pub(crate) token: Option<SecretString>,
}

pub(crate) struct AuthImportDecision<'a> {
    default_auth: Option<AuthSelection>,
    auth_manifest: Option<&'a AuthManifest>,
    provider_id: &'a str,
    interactive: bool,
    yes: bool,
}

impl<'a> AuthImportDecision<'a> {
    pub(crate) fn new(
        default_auth: Option<AuthSelection>,
        auth_manifest: Option<&'a AuthManifest>,
        provider_id: &'a str,
        interactive: bool,
        yes: bool,
    ) -> Self {
        Self {
            default_auth,
            auth_manifest,
            provider_id,
            interactive,
            yes,
        }
    }

    pub(crate) fn resolve(self) -> anyhow::Result<ImportOutcome> {
        if self.default_auth.is_none() {
            return Ok(ImportOutcome {
                auth: None,
                token: None,
            });
        }
        if !self.interactive {
            return Ok(ImportOutcome {
                auth: self.default_auth,
                token: None,
            });
        }
        let detected = detect::detect(self.provider_id);
        if detected.is_empty() {
            return Ok(ImportOutcome {
                auth: self.default_auth,
                token: None,
            });
        }
        let Some(token) = self.prompt_for_import(&detected)? else {
            return Ok(ImportOutcome {
                auth: self.default_auth,
                token: None,
            });
        };

        let auth_manifest = self.auth_manifest;
        let provider_id = self.provider_id;
        let default_auth = self.default_auth.expect("checked default auth presence");
        Ok(ImportOutcome {
            auth: Some(default_auth.promote_imported_static(auth_manifest, provider_id)?),
            token: Some(token),
        })
    }

    /// Semantics:
    /// - Single detected env credential: prints the credential, asks `[y/N/o for OAuth]`.
    ///   Default is N (start OAuth). `y` imports; `N` or `o` falls through.
    /// - Single detected `GhCli` credential: prints the credential, asks
    ///   `[Y/n/o for OAuth]`. Default is Y (import). `n` or `o` falls through.
    ///   For write-capable scopes, the warning is printed before the prompt.
    /// - Multiple detected credentials: uses `inquire::Select` for the user to pick one,
    ///   with OAuth as the last option (default).
    /// - `yes` flag: silently accepts the first detected credential without prompting.
    fn prompt_for_import(
        &self,
        detected: &[detect::DetectedCredential],
    ) -> anyhow::Result<Option<SecretString>> {
        if self.yes {
            return Ok(Some(imported_token_with_notice(&detected[0])));
        }

        if detected.len() == 1 {
            return prompt_single_import(&detected[0]);
        }

        let options: Vec<String> = detected
            .iter()
            .enumerate()
            .map(|(i, credential)| format!("[{}] {}", i + 1, credential_label(credential)))
            .chain(std::iter::once(format!(
                "[{}] OAuth (default)",
                detected.len() + 1
            )))
            .collect();
        let oauth_choice = options.len() - 1;

        print_detected_header(detected);
        anstream::println!();

        let choice = inquire::Select::new("Import existing credential or start OAuth?", options)
            .with_starting_cursor(oauth_choice)
            .prompt_skippable()
            .map_err(|e| anyhow::anyhow!("prompt error: {e}"))?;

        let Some(chosen) = choice else {
            return Ok(None);
        };

        for (i, cred) in detected.iter().enumerate() {
            let label = format!("[{}]", i + 1);
            if chosen.starts_with(&label) {
                return token_after_optional_gh_confirmation(cred);
            }
        }
        Ok(None)
    }
}

fn imported_token_with_notice(credential: &detect::DetectedCredential) -> SecretString {
    match credential {
        detect::DetectedCredential::EnvVar { name, value } => {
            anstream::println!("Importing credential from ${name} (--yes).");
            value.clone()
        },
        detect::DetectedCredential::GhCli { account, token, .. } => {
            anstream::println!("Importing credential from gh CLI (@{account}) (--yes).");
            token.clone()
        },
    }
}

fn prompt_single_import(cred: &detect::DetectedCredential) -> anyhow::Result<Option<SecretString>> {
    match cred {
        detect::DetectedCredential::EnvVar { name, value } => {
            anstream::println!("Detected:");
            anstream::println!("  • ${name} in environment");
            anstream::println!();
            let answer = inquire::Text::new("Import existing credential? [y/N/o for OAuth]")
                .with_default("N")
                .prompt()
                .map_err(|e| anyhow::anyhow!("prompt error: {e}"))?;
            if answer.trim().eq_ignore_ascii_case("y") {
                Ok(Some(value.clone()))
            } else {
                Ok(None)
            }
        },
        detect::DetectedCredential::GhCli {
            account,
            scopes,
            token,
        } => {
            anstream::println!("Detected:");
            anstream::println!(
                "  • gh CLI logged in as @{account} (scopes: {})",
                scopes.join(", ")
            );
            anstream::println!();
            if !confirm_gh_import(account, scopes)? {
                return Ok(None);
            }
            Ok(Some(token.clone()))
        },
    }
}

fn token_after_optional_gh_confirmation(
    credential: &detect::DetectedCredential,
) -> anyhow::Result<Option<SecretString>> {
    match credential {
        detect::DetectedCredential::GhCli {
            scopes,
            account,
            token,
        } => {
            if !confirm_gh_import(account, scopes)? {
                return Ok(None);
            }
            Ok(Some(token.clone()))
        },
        detect::DetectedCredential::EnvVar { value, .. } => Ok(Some(value.clone())),
    }
}

fn print_detected_header(detected: &[detect::DetectedCredential]) {
    anstream::println!("Detected:");
    for cred in detected {
        match cred {
            detect::DetectedCredential::EnvVar { name, .. } => {
                anstream::println!("  • ${name} in environment");
            },
            detect::DetectedCredential::GhCli {
                account, scopes, ..
            } => {
                anstream::println!(
                    "  • gh CLI logged in as @{account} (scopes: {})",
                    scopes.join(", ")
                );
            },
        }
    }
}

fn credential_label(cred: &detect::DetectedCredential) -> String {
    match cred {
        detect::DetectedCredential::EnvVar { name, .. } => format!("${name}"),
        detect::DetectedCredential::GhCli { account, .. } => {
            format!("gh CLI (@{account})")
        },
    }
}

fn confirm_gh_import(account: &str, scopes: &[String]) -> anyhow::Result<bool> {
    let write_scopes: Vec<&str> = scopes
        .iter()
        .filter(|s| {
            matches!(
                s.as_str(),
                "repo" | "workflow" | "admin:org" | "delete_repo"
            )
        })
        .map(String::as_str)
        .collect();
    if !write_scopes.is_empty() {
        anstream::println!(
            "These tokens carry write access via '{}' scope(s).",
            write_scopes.join("', '")
        );
        anstream::println!();
    }
    let prompt = format!("Import gh CLI credential (@{account})? [Y/n/o for OAuth]");
    let answer = inquire::Text::new(&prompt)
        .with_default("y")
        .prompt()
        .map_err(|e| anyhow::anyhow!("prompt error: {e}"))?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}
