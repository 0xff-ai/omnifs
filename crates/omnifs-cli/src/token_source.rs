//! Typed source for static-token input. Shared between `omnifs init` and
//! `omnifs auth import`. `--token VALUE` is rejected to keep secrets out
//! of shell history; only `--token -` (stdin) and `--token-env VAR` are
//! accepted. Interactive mode (no flags, terminal stdin) prompts.

use anyhow::{Context, bail};
use secrecy::SecretString;
use std::io::{IsTerminal, Read};

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

    /// Read the token from the resolved source.
    /// Stdin reads to EOF and trims a single trailing newline.
    /// Env reads from the named variable (errors if unset or empty).
    /// Interactive prompts via the inquire Password input (TTY only).
    pub fn read(self) -> anyhow::Result<SecretString> {
        match self {
            Self::Stdin => {
                let mut buf = String::new();
                std::io::stdin()
                    .read_to_string(&mut buf)
                    .context("read token from stdin")?;
                let trimmed = buf.trim_end_matches('\n').trim_end_matches('\r');
                if trimmed.is_empty() {
                    bail!("stdin produced an empty token");
                }
                Ok(SecretString::from(trimmed.to_string()))
            },
            Self::Env(var) => {
                let value =
                    std::env::var(&var).with_context(|| format!("read token from ${var}"))?;
                if value.is_empty() {
                    bail!("${var} is empty");
                }
                Ok(SecretString::from(value))
            },
            Self::Interactive => {
                if !std::io::stdin().is_terminal() {
                    bail!(
                        "no token source and stdin is not a terminal; pass --token - or --token-env VAR"
                    );
                }
                let token = inquire::Password::new("Token")
                    .with_display_mode(inquire::PasswordDisplayMode::Hidden)
                    .without_confirmation()
                    .prompt()
                    .context("read token from prompt")?;
                if token.is_empty() {
                    bail!("token cannot be empty");
                }
                Ok(SecretString::from(token))
            },
        }
    }
}
