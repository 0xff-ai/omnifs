use super::detect;
use crate::auth::AuthSelection;
use omnifs_workspace::authn::AuthManifest;
use secrecy::SecretString;

pub(crate) struct ImportOutcome {
    pub(crate) auth: Option<AuthSelection>,
    pub(crate) token: Option<SecretString>,
}

pub(crate) struct AuthImportDecision<'a> {
    default_auth: Option<AuthSelection>,
    auth_manifest: Option<&'a AuthManifest>,
    provider_name: &'a str,
    interactive: bool,
    yes: bool,
}

impl<'a> AuthImportDecision<'a> {
    pub(crate) fn new(
        default_auth: Option<AuthSelection>,
        auth_manifest: Option<&'a AuthManifest>,
        provider_name: &'a str,
        interactive: bool,
        yes: bool,
    ) -> Self {
        Self {
            default_auth,
            auth_manifest,
            provider_name,
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
        let detected = detect::detect(self.auth_manifest);
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
        let provider_name = self.provider_name;
        let default_auth = self.default_auth.expect("checked default auth presence");
        Ok(ImportOutcome {
            auth: Some(default_auth.promote_imported_static(auth_manifest, provider_name)?),
            token: Some(token),
        })
    }

    /// Semantics:
    /// - Single detected credential: prints the credential, asks
    ///   `[y/N/o for OAuth]`. Default is N (start OAuth). `y` imports; `N` or
    ///   `o` falls through. The host treats every ambient source the same
    ///   way regardless of kind (env var or command): only the provider
    ///   declares where to look, never how much to trust what it finds.
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
                return Ok(Some(credential_value(cred)));
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
        detect::DetectedCredential::Command { note, value } => {
            anstream::println!("Importing credential from {note} (--yes).");
            value.clone()
        },
    }
}

fn prompt_single_import(cred: &detect::DetectedCredential) -> anyhow::Result<Option<SecretString>> {
    anstream::println!("Detected:");
    anstream::println!("  • {}", credential_label(cred));
    anstream::println!();
    let answer = inquire::Text::new("Import existing credential? [y/N/o for OAuth]")
        .with_default("N")
        .prompt()
        .map_err(|e| anyhow::anyhow!("prompt error: {e}"))?;
    if answer.trim().eq_ignore_ascii_case("y") {
        Ok(Some(credential_value(cred)))
    } else {
        Ok(None)
    }
}

fn print_detected_header(detected: &[detect::DetectedCredential]) {
    anstream::println!("Detected:");
    for cred in detected {
        anstream::println!("  • {}", credential_label(cred));
    }
}

fn credential_label(cred: &detect::DetectedCredential) -> String {
    match cred {
        detect::DetectedCredential::EnvVar { name, .. } => format!("${name} in environment"),
        detect::DetectedCredential::Command { note, .. } => note.clone(),
    }
}

fn credential_value(cred: &detect::DetectedCredential) -> SecretString {
    let (detect::DetectedCredential::EnvVar { value, .. }
    | detect::DetectedCredential::Command { value, .. }) = cred;
    value.clone()
}
