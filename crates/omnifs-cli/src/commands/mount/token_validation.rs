use anyhow::Context;
use omnifs_auth::TokenValidation;
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::Duration;

#[derive(Debug, Default, Clone)]
pub(super) struct ValidationOutcome {
    pub(super) identity: Option<String>,
    pub(super) workspace: Option<String>,
    pub(super) extras: BTreeMap<String, String>,
}

pub(super) async fn validate_static_token(
    validation: &TokenValidation,
    header_name: &str,
    header_prefix: &str,
    token: &str,
    output: &crate::ui::output::Output,
) -> anyhow::Result<ValidationOutcome> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent(concat!("omnifs-cli/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("build HTTP client")?;
    let method = reqwest::Method::from_bytes(validation.method.as_bytes())
        .with_context(|| format!("invalid HTTP method `{}`", validation.method))?;
    let header_value = format!("{header_prefix}{token}");

    let mut req = client
        .request(method, &validation.url)
        .header(header_name, header_value);
    if let Some(body) = validation.body.as_deref() {
        req = req
            .header("Content-Type", "application/json")
            .body(body.to_string());
    }

    output.narrate(format!("validating against {}", validation.url));
    let response = req.send().await.context("validation request failed")?;
    let status = response.status();
    if u32::from(status.as_u16()) != u32::from(validation.expect_status) {
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!(
            "validation failed: expected status {}, got {} ({}). Response: {}",
            validation.expect_status,
            status.as_u16(),
            status.canonical_reason().unwrap_or("unknown"),
            crate::ui::truncate(&body, 300)
        );
    }
    let body = response.text().await.context("read response body")?;
    let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    if let Some(pointer) = validation.json_pointer.as_deref()
        && parsed.pointer(pointer).is_none()
    {
        anyhow::bail!(
            "validation failed: response did not contain `{}`. Response: {}",
            pointer,
            crate::ui::truncate(&body, 300)
        );
    }
    Ok(validation.extract.iter().fold(
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
    ))
}

fn json_to_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}
