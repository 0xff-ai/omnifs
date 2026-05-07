use omnifs_sdk::Cx;
use omnifs_sdk::http::ResponseExt;
use omnifs_sdk::prelude::{ProviderError, ProviderErrorKind, Result};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use url::form_urlencoded::Serializer;

use crate::State;
use crate::types::{CidText, IpnsName};

pub(crate) struct IpfsApi<'cx> {
    cx: &'cx Cx<State>,
    base_url: String,
    ipns_resolve_timeout_secs: u64,
}

impl<'cx> IpfsApi<'cx> {
    pub(crate) fn new(cx: &'cx Cx<State>) -> Self {
        let (base_url, ipns_resolve_timeout_secs) = cx.state(|state| {
            (
                state.config.api_base_url.clone(),
                state.config.ipns_resolve_timeout_secs,
            )
        });
        Self {
            cx,
            base_url,
            ipns_resolve_timeout_secs,
        }
    }

    /// Issue `block/stat` and `dag/stat` for the same CID in a single
    /// callout batch. The two RPCs are independent, so parallelizing them
    /// halves the worst-case latency of `inspect_cid`.
    pub(crate) async fn block_and_dag_stat(&self, cid: &CidText) -> Result<(BlockStat, DagStat)> {
        let arg = cid.to_string();
        let block_url = build_rpc_url(&self.base_url, "block/stat", &[("arg", &arg)]);
        let dag_url = build_rpc_url(&self.base_url, "dag/stat", &[("arg", &arg)]);
        let mut responses = omnifs_sdk::prelude::join_all([
            self.cx.http().get(block_url).send(),
            self.cx.http().get(dag_url).send(),
        ])
        .await
        .into_iter();
        let block = parse_rpc_response::<BlockStat>("block/stat", responses.next().unwrap())?;
        let dag = parse_rpc_response::<DagStat>("dag/stat", responses.next().unwrap())?;
        Ok((block, dag))
    }

    pub(crate) async fn ls(&self, ipfs_path: &str) -> Result<LsObject> {
        let response: LsResponse = self
            .rpc_json(
                "ls",
                &[
                    ("arg", ipfs_path),
                    ("resolve-type", "true"),
                    ("size", "true"),
                ],
            )
            .await?;
        response.objects.into_iter().next().ok_or_else(|| {
            ProviderError::internal(format!("ls returned no object for {ipfs_path}"))
        })
    }

    pub(crate) async fn try_ls(&self, ipfs_path: &str) -> Result<Option<LsObject>> {
        match self.ls(ipfs_path).await {
            Ok(object) => Ok(Some(object)),
            Err(error)
                if matches!(
                    error.kind(),
                    ProviderErrorKind::NotFound | ProviderErrorKind::NotADirectory
                ) =>
            {
                Ok(None)
            },
            Err(error) => Err(error),
        }
    }

    pub(crate) async fn cat(&self, ipfs_path: &str) -> Result<Vec<u8>> {
        self.rpc_bytes("cat", &[("arg", ipfs_path), ("progress", "false")])
            .await
    }

    pub(crate) async fn probe_cat(&self, ipfs_path: &str) -> Result<Option<Vec<u8>>> {
        match self
            .rpc_bytes(
                "cat",
                &[("arg", ipfs_path), ("length", "1"), ("progress", "false")],
            )
            .await
        {
            Ok(bytes) => Ok(Some(bytes)),
            // Without response-body introspection we can't distinguish Kubo's
            // "is a directory" from "doesn't exist" from "node offline." All
            // 4xx/5xx that map to the kinds below collapse to "not a file."
            Err(error)
                if matches!(
                    error.kind(),
                    ProviderErrorKind::NotFound
                        | ProviderErrorKind::NotAFile
                        | ProviderErrorKind::Network
                ) =>
            {
                Ok(None)
            },
            Err(error) => Err(error),
        }
    }

    pub(crate) async fn resolve_ipns(&self, name: &IpnsName) -> Result<String> {
        let arg = format!("/ipns/{name}");
        let timeout = format!("{}s", self.ipns_resolve_timeout_secs);
        let response: ResolveResponse = self
            .rpc_json(
                "resolve",
                &[
                    ("arg", &arg),
                    ("recursive", "true"),
                    ("dht-timeout", &timeout),
                ],
            )
            .await?;
        Ok(response.path)
    }

    pub(crate) async fn pin_list(&self) -> Result<Vec<String>> {
        let response: PinLsResponse = self
            .rpc_json("pin/ls", &[("type", "recursive"), ("quiet", "true")])
            .await?;
        Ok(response.keys.into_keys().collect())
    }

    pub(crate) async fn key_list(&self) -> Result<Vec<String>> {
        let response: KeyListResponse = self.rpc_json("key/list", &[]).await?;
        Ok(response.keys.into_iter().map(|k| k.name).collect())
    }

    async fn rpc_json<T: DeserializeOwned>(&self, cmd: &str, query: &[(&str, &str)]) -> Result<T> {
        let body = self.rpc_bytes(cmd, query).await?;
        parse_json_body(cmd, &body)
    }

    async fn rpc_bytes(&self, cmd: &str, query: &[(&str, &str)]) -> Result<Vec<u8>> {
        let url = build_rpc_url(&self.base_url, cmd, query);
        let response = self.cx.http().get(url).send().await?;
        Ok(response.error_for_status()?.into_body())
    }
}

fn parse_json_body<T: DeserializeOwned>(cmd: &str, body: &[u8]) -> Result<T> {
    serde_json::from_slice(body).map_err(|error| {
        ProviderError::invalid_input(format!("{cmd} returned invalid JSON: {error}"))
    })
}

fn parse_rpc_response<T: DeserializeOwned>(
    cmd: &str,
    response: Result<http::Response<Vec<u8>>>,
) -> Result<T> {
    let response = response?.error_for_status()?;
    parse_json_body(cmd, response.body())
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct BlockStat {
    #[serde(rename = "Size", deserialize_with = "deserialize_u64")]
    pub(crate) size: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct DagStat {
    #[serde(
        rename = "TotalSize",
        default,
        deserialize_with = "deserialize_optional_u64"
    )]
    total_size: Option<u64>,
    #[serde(rename = "DagStats", default)]
    dag_stats: Vec<DagStatEntry>,
}

impl DagStat {
    pub(crate) fn total_size(&self) -> u64 {
        self.total_size
            .or_else(|| self.dag_stats.first().map(|entry| entry.size))
            .unwrap_or_default()
    }
}

#[derive(Clone, Debug, Deserialize)]
struct DagStatEntry {
    #[serde(rename = "Size", deserialize_with = "deserialize_u64")]
    size: u64,
}

#[derive(Clone, Debug, Deserialize)]
struct LsResponse {
    #[serde(rename = "Objects", default)]
    objects: Vec<LsObject>,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct LsObject {
    #[serde(rename = "Links", default)]
    pub(crate) links: Vec<LsLink>,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct LsLink {
    #[serde(rename = "Name")]
    pub(crate) name: String,
    #[serde(rename = "Size", deserialize_with = "deserialize_u64")]
    pub(crate) size: u64,
    #[serde(rename = "Type", deserialize_with = "deserialize_i32")]
    kind: i32,
}

impl LsLink {
    // Kubo's UnixFS link `Type` field uses `1` for legacy directory and
    // `5` for HAMT-sharded directory; everything else (file, raw block,
    // symlink, unknown) is a file from the projection's perspective.
    pub(crate) fn is_directory(&self) -> bool {
        matches!(self.kind, 1 | 5)
    }
}

#[derive(Clone, Debug, Deserialize)]
struct ResolveResponse {
    #[serde(rename = "Path")]
    path: String,
}

#[derive(Clone, Debug, Deserialize)]
struct PinLsResponse {
    #[serde(rename = "Keys", default)]
    keys: std::collections::BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, Deserialize)]
struct KeyListResponse {
    #[serde(rename = "Keys", default)]
    keys: Vec<KeyEntry>,
}

#[derive(Clone, Debug, Deserialize)]
struct KeyEntry {
    #[serde(rename = "Name")]
    name: String,
}

fn build_rpc_url(base_url: &str, cmd: &str, query: &[(&str, &str)]) -> String {
    let mut url = format!(
        "{}/{}",
        base_url.trim_end_matches('/'),
        cmd.trim_start_matches('/')
    );
    if query.is_empty() {
        return url;
    }
    let mut serializer = Serializer::new(String::new());
    for (name, value) in query {
        serializer.append_pair(name, value);
    }
    url.push('?');
    url.push_str(&serializer.finish());
    url
}

fn deserialize_u64<'de, D>(deserializer: D) -> core::result::Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Value {
        U64(u64),
        I64(i64),
        String(String),
    }
    match Value::deserialize(deserializer)? {
        Value::U64(value) => Ok(value),
        Value::I64(value) => u64::try_from(value).map_err(serde::de::Error::custom),
        Value::String(value) => value.parse().map_err(serde::de::Error::custom),
    }
}

fn deserialize_optional_u64<'de, D>(deserializer: D) -> core::result::Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<serde_json::Value>::deserialize(deserializer)?.map_or(Ok(None), |value| {
        parse_json_u64(value)
            .map(Some)
            .map_err(serde::de::Error::custom)
    })
}

fn deserialize_i32<'de, D>(deserializer: D) -> core::result::Result<i32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Value {
        I32(i32),
        I64(i64),
        String(String),
    }
    match Value::deserialize(deserializer)? {
        Value::I32(value) => Ok(value),
        Value::I64(value) => i32::try_from(value).map_err(serde::de::Error::custom),
        Value::String(value) => value.parse().map_err(serde::de::Error::custom),
    }
}

fn parse_json_u64(value: serde_json::Value) -> core::result::Result<u64, String> {
    match value {
        serde_json::Value::Number(number) => number
            .as_u64()
            .ok_or_else(|| String::from("expected u64 number")),
        serde_json::Value::String(value) => value.parse::<u64>().map_err(|error| error.to_string()),
        other => Err(format!("expected u64-compatible value, got {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_rpc_url_repeats_arg_pairs() {
        let url = build_rpc_url(
            "https://kubo.test/api/v0",
            "resolve",
            &[
                ("arg", "/ipns/example.com"),
                ("arg", "/ipfs/bafy"),
                ("recursive", "true"),
            ],
        );
        assert_eq!(
            url,
            "https://kubo.test/api/v0/resolve?arg=%2Fipns%2Fexample.com&arg=%2Fipfs%2Fbafy&recursive=true"
        );
    }

    #[test]
    fn build_rpc_url_trims_trailing_slash_in_base() {
        let url = build_rpc_url("https://kubo.test/api/v0/", "block/stat", &[]);
        assert_eq!(url, "https://kubo.test/api/v0/block/stat");
    }
}
