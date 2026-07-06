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
        let detected = detect::detect(self.auth_manifest);

        // `--yes` accepts a detected ambient credential without prompting, even
        // non-interactively, so the documented behavior is reachable in scripts.
        if self.yes {
            if let Some(credential) = detected.first() {
                anstream::eprintln!(
                    "{}",
                    crate::ui::ok(
                        "credential",
                        format!("imported from {}", credential_source(credential))
                    )
                );
                return self.promote(credential_value(credential));
            }
            return Ok(ImportOutcome {
                auth: self.default_auth,
                token: None,
            });
        }

        if !self.interactive || detected.is_empty() {
            return Ok(ImportOutcome {
                auth: self.default_auth,
                token: None,
            });
        }
        let Some(token) = Self::prompt_for_import(&detected)? else {
            return Ok(ImportOutcome {
                auth: self.default_auth,
                token: None,
            });
        };
        self.promote(token)
    }

    fn promote(&self, token: SecretString) -> anyhow::Result<ImportOutcome> {
        let default_auth = self
            .default_auth
            .clone()
            .expect("checked default auth presence");
        Ok(ImportOutcome {
            auth: Some(
                default_auth.promote_imported_static(self.auth_manifest, self.provider_name)?,
            ),
            token: Some(token),
        })
    }

    /// Interactive import decision. A single detected credential offers three
    /// honest options; multiple credentials use a picker with a sign-in fallback.
    /// The host treats every ambient source the same way regardless of kind
    /// (env var or command): only the provider declares where to look, never how
    /// much to trust what it finds.
    fn prompt_for_import(
        detected: &[detect::DetectedCredential],
    ) -> anyhow::Result<Option<SecretString>> {
        if detected.len() == 1 {
            return prompt_single_import(&detected[0]);
        }

        let mut options: Vec<String> = detected
            .iter()
            .map(|credential| format!("import from {}", credential_source(credential)))
            .collect();
        options.push("sign in with OAuth instead".to_string());
        options.push("skip auth for now".to_string());

        // `.prompt()` so ESC/ctrl-c cancels the whole command; declining is the
        // explicit "skip auth for now" option, never a silent fallback.
        let chosen = inquire::Select::new("How should this mount authenticate?", options.clone())
            .prompt()
            .map_err(crate::ui::from_inquire)?;

        for (i, cred) in detected.iter().enumerate() {
            if chosen == options[i] {
                return Ok(Some(credential_value(cred)));
            }
        }
        Ok(None)
    }
}

fn prompt_single_import(cred: &detect::DetectedCredential) -> anyhow::Result<Option<SecretString>> {
    let import = format!("import from {}", credential_source(cred));
    let options = vec![
        import.clone(),
        "sign in with OAuth instead".to_string(),
        "skip auth for now".to_string(),
    ];
    // `.prompt()` so ESC/ctrl-c cancels the whole command; declining is the
    // explicit "skip auth for now" option, never a silent fallback.
    let answer = inquire::Select::new("How should this mount authenticate?", options)
        .prompt()
        .map_err(crate::ui::from_inquire)?;
    if answer == import {
        Ok(Some(credential_value(cred)))
    } else {
        Ok(None)
    }
}

/// Where a detected credential comes from, for display. Env vars render as
/// `$NAME`; a command renders its provider-supplied note.
fn credential_source(cred: &detect::DetectedCredential) -> String {
    match cred {
        detect::DetectedCredential::EnvVar { name, .. } => format!("${name}"),
        detect::DetectedCredential::Command { note, .. } => note.clone(),
    }
}

fn credential_value(cred: &detect::DetectedCredential) -> SecretString {
    let (detect::DetectedCredential::EnvVar { value, .. }
    | detect::DetectedCredential::Command { value, .. }) = cred;
    value.clone()
}
