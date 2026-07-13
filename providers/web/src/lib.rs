#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use std::fmt;
use std::str::FromStr;

use dom_smoothie::{Config as ReadabilityConfig, Readability, TextMode};
use omnifs_sdk::http::ResponseExt;
use omnifs_sdk::prelude::*;
use percent_encoding::percent_decode_str;

#[omnifs_sdk::config]
struct Config {
    #[serde(default)]
    domains: Vec<String>,
}

#[derive(Clone)]
struct State {
    domains: Vec<Host>,
}

#[omnifs_sdk::path_segment(validate = is_domain_segment, normalize = str::to_ascii_lowercase)]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Host(String);

#[derive(Debug, Clone, PartialEq, Eq)]
struct WebPath {
    encoded: String,
    decoded: String,
}

#[omnifs_sdk::path_captures]
struct WebPathKey {
    path: WebPath,
}

#[omnifs_sdk::provider(
    id = "web",
    display_name = "Web",
    description = "configured web pages, fetched as files",
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
        let mut domains = config
            .domains
            .into_iter()
            .map(|domain| {
                domain.parse::<Host>().map_err(|()| {
                    ProviderError::invalid_input(format!(
                        "invalid configured domain {domain:?}; use hostnames only, without scheme, port, path, or wildcard"
                    ))
                })
            })
            .collect::<Result<Vec<_>>>()?;
        domains.sort();
        domains.dedup();

        r.dir("/https").handler(list_hosts)?;
        r.dir("/raw/https").handler(list_hosts)?;

        for host in domains.iter().cloned() {
            r.dir(format!("/https/{host}")).handler(open_host)?;
            r.dir(format!("/raw/https/{host}")).handler(open_host)?;

            let markdown_host = host.clone();
            r.file(format!("/https/{host}/{{path}}")).handler(
                move |cx: Cx<State>, key: WebPathKey| {
                    let host = markdown_host.clone();
                    async move { markdown(cx, host, key.path).await }
                },
            )?;

            let raw_host = host;
            r.file(format!("/raw/https/{raw_host}/{{path}}")).handler(
                move |cx: Cx<State>, key: WebPathKey| {
                    let host = raw_host.clone();
                    async move { raw(cx, host, key.path).await }
                },
            )?;
        }

        Ok(State { domains })
    }
}

async fn list_hosts(cx: DirCx<State>) -> Result<DirListing> {
    let entries = cx.state(|state| {
        state
            .domains
            .iter()
            .map(|host| Entry::dir(host.to_string()))
            .collect::<Vec<_>>()
    });
    Ok(DirListing::exhaustive(entries))
}

async fn open_host(_cx: DirCx<State>) -> Result<DirListing> {
    Ok(DirListing::open(std::iter::empty::<Entry>()))
}

async fn markdown(cx: Cx<State>, host: Host, path: WebPath) -> Result<FileProjection> {
    let url = path.url(&host);
    let response = cx.http().get(&url).send().await?.error_for_status()?;
    let html = String::from_utf8_lossy(response.body());
    let config = ReadabilityConfig {
        text_mode: TextMode::Markdown,
        ..Default::default()
    };
    let mut readability = Readability::new(html.as_ref(), Some(&url), Some(config))
        .map_err(|error| ProviderError::invalid_input(format!("readability setup: {error}")))?;
    let article = readability
        .parse()
        .map_err(|error| ProviderError::invalid_input(format!("readability parse: {error}")))?;
    let markdown = format_markdown(&article.title, article.text_content.as_ref());
    Ok(FileProjection::dynamic_body_with_type(
        markdown.into_bytes(),
        ContentType::Markdown,
    ))
}

async fn raw(cx: Cx<State>, host: Host, path: WebPath) -> Result<FileProjection> {
    let response = cx
        .http()
        .get(path.url(&host))
        .send()
        .await?
        .error_for_status()?;
    Ok(FileProjection::dynamic_body_with_type(
        response.into_body(),
        ContentType::Octet,
    ))
}

impl WebPath {
    fn url(&self, host: &Host) -> String {
        if self.encoded == "@root" {
            format!("https://{host}/")
        } else {
            format!("https://{host}/{}", self.decoded)
        }
    }
}

impl FromStr for WebPath {
    type Err = ();

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        if value.is_empty() || value.contains('#') || !has_valid_percent_encoding(value) {
            return Err(());
        }

        let (path, query) = value.split_once('?').unwrap_or((value, ""));
        let decoded_path = decode_slashes(path).ok_or(())?;
        if decoded_path.starts_with('/') || decoded_path.split('/').any(is_traversal_segment) {
            return Err(());
        }
        let decoded = if value.contains('?') {
            format!("{decoded_path}?{query}")
        } else {
            decoded_path
        };
        Ok(Self {
            encoded: value.to_string(),
            decoded,
        })
    }
}

impl fmt::Display for WebPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.encoded)
    }
}

fn has_valid_percent_encoding(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit()
            {
                return false;
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    true
}

fn decode_slashes(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = String::with_capacity(value.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hi = hex_value(bytes[index + 1])?;
            let lo = hex_value(bytes[index + 2])?;
            if hi == 2 && lo == 15 {
                decoded.push('/');
            } else {
                decoded.push('%');
                decoded.push(char::from(bytes[index + 1]));
                decoded.push(char::from(bytes[index + 2]));
            }
            index += 3;
        } else {
            decoded.push(char::from(bytes[index]));
            index += 1;
        }
    }
    Some(decoded)
}

fn is_traversal_segment(segment: &str) -> bool {
    if segment == "." || segment == ".." {
        return true;
    }
    percent_decode_str(segment)
        .decode_utf8()
        .is_ok_and(|decoded| decoded == "." || decoded == "..")
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
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
