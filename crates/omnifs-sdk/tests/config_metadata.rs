use omnifs_sdk::{HostFile, HostSocket, ProvidesConfigMetadata};
use std::collections::BTreeMap;

#[allow(dead_code)]
#[omnifs_sdk::config]
struct Resolver {
    url: String,
    #[serde(default)]
    aliases: Vec<String>,
}

#[allow(dead_code)]
#[omnifs_sdk::config]
struct Config {
    /// Docker daemon endpoint.
    #[omnifs(default = "unix:///var/run/docker.sock")]
    #[serde(default = "default_endpoint")]
    endpoint: HostSocket,
    /// Host path to the database file.
    path: HostFile,
    #[omnifs(default = 20)]
    #[serde(default)]
    sample_limit: u32,
    #[serde(default)]
    resolvers: BTreeMap<String, Resolver>,
}

fn default_endpoint() -> HostSocket {
    HostSocket("unix:///var/run/docker.sock".to_string())
}

#[test]
fn config_metadata_is_generated_from_the_static_dialect() {
    const METADATA: omnifs_sdk::ConfigMetadata = match <Config as ProvidesConfigMetadata>::METADATA
    {
        Some(metadata) => metadata,
        None => panic!("config metadata missing"),
    };
    static BYTES: [u8; omnifs_sdk::METADATA_JSON_CAPACITY] = METADATA.json_bytes();
    let metadata: serde_json::Value = serde_json::from_slice(&BYTES).unwrap();
    let fields = metadata["fields"].as_array().unwrap();

    assert_eq!(fields[0]["name"], "endpoint");
    assert_eq!(fields[0]["type"], serde_json::json!({ "kind": "string" }));
    assert_eq!(
        fields[0]["binding"],
        serde_json::json!({ "kind": "socket" }),
    );
    assert_eq!(fields[0]["default"], "unix:///var/run/docker.sock");

    assert_eq!(fields[1]["name"], "path");
    assert_eq!(fields[1]["required"], true);
    assert_eq!(fields[1]["binding"], serde_json::json!({ "kind": "file" }),);

    assert_eq!(fields[2]["name"], "sample_limit");
    assert_eq!(fields[2]["type"], serde_json::json!({ "kind": "integer" }));
    assert_eq!(fields[2]["default"], 20);

    assert_eq!(fields[3]["name"], "resolvers");
    assert_eq!(fields[3]["type"]["kind"], "map");
    let resolver_fields = fields[3]["type"]["values"]["fields"].as_array().unwrap();
    assert_eq!(resolver_fields[0]["name"], "url");
    assert_eq!(resolver_fields[0]["required"], true);
    assert_eq!(
        resolver_fields[1]["type"],
        serde_json::json!({
            "kind": "array",
            "items": { "kind": "string" },
        }),
    );
}
