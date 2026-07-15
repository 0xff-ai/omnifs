use omnifs_sdk::serde_json::json;
use omnifs_sdk::{ConfigMetadataBytes, ConfigType, HostFile, HostResourceBinding, HostSocket};
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
    let fields: Vec<omnifs_sdk::ConfigField> =
        omnifs_sdk::serde_json::from_slice(Config::JSON).expect("config metadata missing");

    assert_eq!(fields[0].name, "endpoint");
    assert_eq!(fields[0].value_type, ConfigType::String);
    assert_eq!(fields[0].binding, Some(HostResourceBinding::Socket));
    assert_eq!(
        fields[0].default,
        Some(json!("unix:///var/run/docker.sock"))
    );

    assert_eq!(fields[1].name, "path");
    assert!(fields[1].required);
    assert_eq!(
        fields[1].binding,
        Some(HostResourceBinding::File {
            mode: omnifs_sdk::PreopenMode::default()
        })
    );

    assert_eq!(fields[2].name, "sample_limit");
    assert_eq!(fields[2].value_type, ConfigType::Integer);
    assert_eq!(fields[2].default, Some(json!(20)));

    assert_eq!(fields[3].name, "resolvers");
    let ConfigType::Map { values } = &fields[3].value_type else {
        panic!("resolvers should be a map");
    };
    let ConfigType::Object {
        fields: resolver_fields,
    } = values.as_ref()
    else {
        panic!("map values should be an object");
    };
    assert_eq!(resolver_fields[0].name, "url");
    assert!(resolver_fields[0].required);
    assert_eq!(resolver_fields[1].name, "aliases");
    assert_eq!(
        resolver_fields[1].value_type,
        ConfigType::Array {
            items: Box::new(ConfigType::String)
        }
    );
}
