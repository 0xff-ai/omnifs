#![allow(clippy::disallowed_macros)] // migrates in wave 2 (cli-redesign)
use anyhow::Context;
use omnifs_workspace::authn::TokenValidation;
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::Duration;

#[derive(Debug, Default, Clone)]
pub(super) struct ValidationOutcome {
    pub(super) identity: Option<String>,
    pub(super) workspace: Option<String>,
    pub(super) extras: BTreeMap<String, String>,
}

pub(super) struct StaticTokenValidator<'a> {
    validation: &'a TokenValidation,
    header_name: &'a str,
    header_prefix: &'a str,
}

impl<'a> StaticTokenValidator<'a> {
    pub(super) fn new(
        validation: &'a TokenValidation,
        header_name: &'a str,
        header_prefix: &'a str,
    ) -> Self {
        Self {
            validation,
            header_name,
            header_prefix,
        }
    }

    pub(super) async fn validate(&self, token: &str) -> anyhow::Result<ValidationOutcome> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent(concat!("omnifs-cli/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("build HTTP client")?;
        let method = reqwest::Method::from_bytes(self.validation.method.as_bytes())
            .with_context(|| format!("invalid HTTP method `{}`", self.validation.method))?;
        let header_value = format!("{}{token}", self.header_prefix);

        let mut req = client
            .request(method, &self.validation.url)
            .header(self.header_name, header_value);
        if let Some(body) = self.validation.body.as_deref() {
            req = req
                .header("Content-Type", "application/json")
                .body(body.to_string());
        }

        anstream::eprintln!(
            "{}",
            crate::ui::note(format!("validating against {}", self.validation.url))
        );
        let response = req.send().await.context("validation request failed")?;
        let status = response.status();
        if u32::from(status.as_u16()) != u32::from(self.validation.expect_status) {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!(
                "validation failed: expected status {}, got {} ({}). Response: {}",
                self.validation.expect_status,
                status.as_u16(),
                status.canonical_reason().unwrap_or("unknown"),
                crate::ui::truncate(&body, 300)
            );
        }
        let body = response.text().await.context("read response body")?;
        let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        if let Some(pointer) = self.validation.json_pointer.as_deref()
            && parsed.pointer(pointer).is_none()
        {
            anyhow::bail!(
                "validation failed: response did not contain `{}`. Response: {}",
                pointer,
                crate::ui::truncate(&body, 300)
            );
        }
        Ok(self.outcome_from(&parsed))
    }

    fn outcome_from(&self, parsed: &Value) -> ValidationOutcome {
        self.validation.extract.iter().fold(
            ValidationOutcome::default(),
            |mut outcome, (key, pointer)| {
                if let Some(val) = parsed.pointer(pointer).and_then(json_to_string) {
                    match key.as_str() {
                        "identity" => outcome.identity = Some(val),
                        "workspace" => outcome.workspace = Some(val),
                        _ => {
                            outcome.extras.insert(key.clone(), val);
                        },
                    }
                }
                outcome
            },
        )
    }
}

fn json_to_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}
