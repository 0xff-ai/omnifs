#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
#![allow(clippy::needless_pass_by_value)]

use std::fmt;
use std::str::FromStr;

use dom_smoothie::{Config as ReadabilityConfig, Readability, TextMode};
use omnifs_sdk::http::ResponseExt;
use omnifs_sdk::prelude::*;

#[derive(Clone, Debug)]
struct State;

#[omnifs_sdk::config]
struct Config {
    #[serde(default)]
    domains: Vec<String>,
}

#[omnifs_sdk::path_segment(validate = is_domain_segment, normalize = str::to_ascii_lowercase)]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Host(String);

#[derive(Debug, Clone, PartialEq, Eq)]
struct WebPath(String);

#[omnifs_sdk::path_captures]
struct WebKey {
    host: Host,
    rest: WebPath,
}

#[omnifs_sdk::provider(
    id = "web",
    display_name = "Web",
    mount = "web",
    capabilities(domain(
        dynamic,
        "Fetch only the HTTPS domains enumerated by each mount's `domains` config."
    )),
    limits(memory_mb(
        128,
        "Leave room for HTML parsing and readability extraction in the provider guest."
    ),)
)]
impl WebProvider {
    fn start(config: Config, r: &mut Router<State>) -> Result<State> {
        for domain in &config.domains {
            let _: Host = domain.parse().map_err(|()| {
                ProviderError::invalid_input(format!(
                    "invalid configured domain {domain:?}; use hostnames only, without scheme, port, path, or wildcard"
                ))
            })?;
        }
        r.file("/https/{host}/{*rest}").handler(markdown)?;
        r.file("/raw/https/{host}/{*rest}").handler(raw)?;
        Ok(State)
    }
}

async fn markdown(cx: Cx<State>, key: WebKey) -> Result<FileProjection> {
    let url = key.url();
    let response = cx.http().get(&url).send().await?.error_for_status()?;
    let html = String::from_utf8_lossy(response.body());
    let markdown = WebKey::markdown(&html, &url)?;
    Ok(FileProjection::dynamic_body_with_type(
        markdown.into_bytes(),
        ContentType::Markdown,
    ))
}

async fn raw(cx: Cx<State>, key: WebKey) -> Result<FileProjection> {
    let response = cx.http().get(key.url()).send().await?.error_for_status()?;
    Ok(FileProjection::dynamic_body_with_type(
        response.into_body(),
        ContentType::Octet,
    ))
}

impl WebKey {
    fn url(&self) -> String {
        self.rest.url(&self.host)
    }

    fn markdown(html: &str, url: &str) -> Result<String> {
        let config = ReadabilityConfig {
            text_mode: TextMode::Markdown,
            ..Default::default()
        };
        let mut readability = Readability::new(html, Some(url), Some(config))
            .map_err(|error| ProviderError::invalid_input(format!("readability setup: {error}")))?;
        let article = readability
            .parse()
            .map_err(|error| ProviderError::invalid_input(format!("readability parse: {error}")))?;
        Ok(format_markdown(
            &article.title,
            article.text_content.as_ref(),
        ))
    }
}

impl WebPath {
    fn url(&self, host: &Host) -> String {
        if self.0.is_empty() {
            format!("https://{host}/")
        } else {
            format!("https://{host}/{}", self.0)
        }
    }
}

impl FromStr for WebPath {
    type Err = ();

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        if value.contains(['?', '#']) {
            return Err(());
        }
        if value
            .split('/')
            .any(|segment| segment == "." || segment == "..")
        {
            return Err(());
        }
        Ok(Self(value.to_string()))
    }
}

impl fmt::Display for WebPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn format_markdown(title: &str, body: &str) -> String {
    let mut markdown = String::new();
    let title = title.trim();
    if !title.is_empty() {
        markdown.push_str("# ");
        markdown.push_str(title);
        markdown.push_str("\n\n");
    }
    markdown.push_str(body.trim());
    markdown.push('\n');
    markdown
}

fn is_domain_segment(value: &str) -> bool {
    if value.is_empty() || value == "*" || value.starts_with('.') || value.ends_with('.') {
        return false;
    }
    value.split('.').all(is_domain_label)
}

fn is_domain_label(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && !value.ends_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}
