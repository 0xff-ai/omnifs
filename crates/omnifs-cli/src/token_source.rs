//! Typed source for static-token input used by `omnifs mount add`. `--token VALUE`
//! is rejected to keep secrets out of shell history; only `--token -` (stdin)
//! and `--token-env VAR` are accepted. Interactive mode (no flags, terminal
//! stdin) prompts.

use anyhow::{Context, bail};
use secrecy::SecretString;
use std::io::Read;

#[derive(Debug, Clone)]
pub enum TokenSource {
    Stdin,
    Env(String),
    Interactive,
}

impl TokenSource {
    /// Resolve a `TokenSource` from the two clap-flag values plus an
    /// interactivity flag. Returns an error for `--token VALUE`
    /// (`--token` is reserved for `-` / stdin) and for non-interactive
    /// with no flags.
    pub fn resolve(
        token: Option<&str>,
        token_env: Option<&str>,
        interactive: bool,
    ) -> anyhow::Result<Self> {
        match (token, token_env) {
            (Some("-"), None) => Ok(Self::Stdin),
            (Some(other), None) => bail!(
                "--token VALUE is rejected to keep secrets out of shell history; \
                 use --token - to read stdin, or --token-env VAR to read an env var \
                 (got --token {other:?})"
            ),
            (None, Some(var)) => Ok(Self::Env(var.to_string())),
            (Some(_), Some(_)) => {
                // Should be unreachable: clap's `conflicts_with` rejects this
                // combination before resolve() is called, but defend in depth.
                bail!("--token and --token-env are mutually exclusive")
            },
            (None, None) if interactive => Ok(Self::Interactive),
            (None, None) => bail!(
                "no token source: pass --token - (stdin) or --token-env VAR, \
                 or run interactively"
            ),
        }
    }

    /// Read the token from the resolved source. Every source trims all
    /// surrounding whitespace and rejects an empty-after-trim value, so a
    /// stray newline or copy-paste padding never becomes part of the secret.
    /// Interactive prompts via the shared hidden password input (TTY only).
    pub fn read(self, output: &crate::ui::output::Output) -> anyhow::Result<SecretString> {
        match self {
            Self::Stdin => {
                let mut buf = String::new();
                std::io::stdin()
                    .read_to_string(&mut buf)
                    .context("read token from stdin")?;
                let trimmed = buf.trim();
                if trimmed.is_empty() {
                    bail!("stdin produced an empty token");
                }
                Ok(SecretString::from(trimmed.to_string()))
            },
            Self::Env(var) => {
                let value =
                    std::env::var(&var).with_context(|| format!("read token from ${var}"))?;
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    bail!("${var} is empty");
                }
                Ok(SecretString::from(trimmed.to_string()))
            },
            Self::Interactive => {
                if !crate::ui::prompt::is_terminal() {
                    bail!(
                        "no token source and stdin is not a terminal; pass --token - or --token-env VAR"
                    );
                }
                let token = crate::ui::prompt::Password::new("Token").ask_with_output(output)?;
                let trimmed = token.trim();
                if trimmed.is_empty() {
                    bail!("token cannot be empty");
                }
                Ok(SecretString::from(trimmed.to_string()))
            },
        }
    }
}
