//! Static provider metadata embedded in provider Wasm components.

use crate::auth::{Auth, Extract, Flow, OAuth, Scheme, SchemeEntry, StaticToken, Validation};
use crate::config_resource::{
    ConfigField, ConfigMetadata, ConfigType, DefaultValue, HostResourceBinding,
};

pub const WIT_CONTRACT: &str = "omnifs:provider@0.4.0";
pub const METADATA_JSON_CAPACITY: usize = 64 * 1024;

/// Provider metadata contract.
pub trait Provider {
    const METADATA: Metadata;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Metadata {
    pub id: &'static str,
    pub display_name: &'static str,
    pub provider: &'static str,
    pub default_mount: &'static str,
    pub version: Option<&'static str>,
    pub build_evidence: Option<BuildEvidence>,
    pub capabilities: &'static [Need],
    pub auth: Option<Auth>,
    pub config: Option<ConfigMetadata>,
}

impl Metadata {
    #[must_use]
    pub const fn new(id: &'static str) -> Self {
        Self {
            id,
            display_name: id,
            provider: "",
            default_mount: id,
            version: None,
            build_evidence: None,
            capabilities: &[],
            auth: None,
            config: None,
        }
    }

    #[must_use]
    pub const fn display_name(mut self, display_name: &'static str) -> Self {
        self.display_name = display_name;
        self
    }

    #[must_use]
    pub const fn provider(mut self, provider: &'static str) -> Self {
        self.provider = provider;
        self
    }

    #[must_use]
    pub const fn mount(mut self, default_mount: &'static str) -> Self {
        self.default_mount = default_mount;
        self
    }

    #[must_use]
    pub const fn version(mut self, version: &'static str) -> Self {
        self.version = Some(version);
        self
    }

    #[must_use]
    pub const fn build_evidence(mut self, evidence: BuildEvidence) -> Self {
        self.build_evidence = Some(evidence);
        self
    }

    #[must_use]
    pub const fn capabilities(mut self, capabilities: &'static [Need]) -> Self {
        self.capabilities = capabilities;
        self
    }

    #[must_use]
    pub const fn auth(mut self, auth: Auth) -> Self {
        self.auth = Some(auth);
        self
    }

    #[must_use]
    pub const fn config(mut self, config: Option<ConfigMetadata>) -> Self {
        self.config = config;
        self
    }

    #[must_use]
    pub const fn json_bytes(&self) -> [u8; METADATA_JSON_CAPACITY] {
        let mut writer = JsonWriter::<METADATA_JSON_CAPACITY>::new();
        write_metadata(&mut writer, self);
        writer.finish()
    }
}

impl ConfigMetadata {
    #[must_use]
    pub const fn json_bytes(&self) -> [u8; METADATA_JSON_CAPACITY] {
        let mut writer = JsonWriter::<METADATA_JSON_CAPACITY>::new();
        write_config_metadata(&mut writer, self);
        writer.finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuildEvidence {
    pub wit: &'static str,
    pub sdk: &'static str,
}

impl BuildEvidence {
    #[must_use]
    pub const fn current(sdk: &'static str) -> Self {
        Self {
            wit: WIT_CONTRACT,
            sdk,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Need {
    Domain {
        value: &'static str,
        why: &'static str,
        dynamic: bool,
    },
    GitRepo {
        value: &'static str,
        why: &'static str,
        dynamic: bool,
    },
    UnixSocket {
        value: &'static str,
        why: &'static str,
        dynamic: bool,
    },
    PreopenedPath {
        host: &'static str,
        guest: &'static str,
        why: &'static str,
        dynamic: bool,
    },
    MemoryMb {
        value: u32,
        why: &'static str,
        dynamic: bool,
    },
}

impl Need {
    #[must_use]
    pub const fn domain(value: &'static str, why: &'static str) -> Self {
        Self::Domain {
            value,
            why,
            dynamic: false,
        }
    }

    #[must_use]
    pub const fn git_repo(value: &'static str, why: &'static str) -> Self {
        Self::GitRepo {
            value,
            why,
            dynamic: false,
        }
    }

    #[must_use]
    pub const fn unix_socket_dynamic(why: &'static str) -> Self {
        Self::UnixSocket {
            value: DYNAMIC_PLACEHOLDER,
            why,
            dynamic: true,
        }
    }

    #[must_use]
    pub const fn preopened_path_dynamic(why: &'static str) -> Self {
        Self::PreopenedPath {
            host: DYNAMIC_PLACEHOLDER,
            guest: DYNAMIC_PLACEHOLDER,
            why,
            dynamic: true,
        }
    }

    #[must_use]
    pub const fn memory_mb(value: u32, why: &'static str) -> Self {
        Self::MemoryMb {
            value,
            why,
            dynamic: false,
        }
    }
}

pub const DYNAMIC_PLACEHOLDER: &str = "resolved from config at mount-start";

struct JsonWriter<const N: usize> {
    bytes: [u8; N],
    pos: usize,
}

impl<const N: usize> JsonWriter<N> {
    const fn new() -> Self {
        Self {
            bytes: [b' '; N],
            pos: 0,
        }
    }

    const fn finish(self) -> [u8; N] {
        self.bytes
    }

    const fn push_byte(&mut self, byte: u8) {
        assert!(self.pos < N, "metadata JSON buffer overflow");
        self.bytes[self.pos] = byte;
        self.pos += 1;
    }

    const fn push_bytes(&mut self, bytes: &[u8]) {
        let mut i = 0;
        while i < bytes.len() {
            self.push_byte(bytes[i]);
            i += 1;
        }
    }

    const fn push_str(&mut self, value: &str) {
        self.push_bytes(value.as_bytes());
    }

    const fn push_json_str(&mut self, value: &str) {
        self.push_byte(b'"');
        let bytes = value.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'"' => self.push_bytes(b"\\\""),
                b'\\' => self.push_bytes(b"\\\\"),
                b'\n' => self.push_bytes(b"\\n"),
                b'\r' => self.push_bytes(b"\\r"),
                b'\t' => self.push_bytes(b"\\t"),
                byte if byte < 0x20 => {
                    self.push_str("\\u00");
                    self.push_hex_nibble(byte >> 4);
                    self.push_hex_nibble(byte & 0x0f);
                },
                byte => self.push_byte(byte),
            }
            i += 1;
        }
        self.push_byte(b'"');
    }

    const fn push_hex_nibble(&mut self, value: u8) {
        self.push_byte(if value < 10 {
            b'0' + value
        } else {
            b'a' + (value - 10)
        });
    }

    const fn push_bool(&mut self, value: bool) {
        if value {
            self.push_str("true");
        } else {
            self.push_str("false");
        }
    }

    const fn push_u64(&mut self, value: u64) {
        let mut divisor = 1u64;
        while value / divisor >= 10 {
            divisor *= 10;
        }
        while divisor > 0 {
            let digit = ((value / divisor) % 10) as u8;
            self.push_byte(b'0' + digit);
            divisor /= 10;
        }
    }

    const fn push_i64(&mut self, value: i64) {
        if value < 0 {
            self.push_byte(b'-');
        }
        self.push_u64(value.unsigned_abs());
    }
}

const fn write_metadata<const N: usize>(writer: &mut JsonWriter<N>, metadata: &Metadata) {
    writer.push_byte(b'{');
    let mut first = true;
    write_str_field(writer, &mut first, "id", metadata.id);
    write_str_field(writer, &mut first, "displayName", metadata.display_name);
    write_str_field(writer, &mut first, "provider", metadata.provider);
    write_str_field(writer, &mut first, "defaultMount", metadata.default_mount);
    if let Some(version) = metadata.version {
        write_str_field(writer, &mut first, "version", version);
    }
    if let Some(evidence) = metadata.build_evidence {
        write_field_name(writer, &mut first, "buildEvidence");
        write_build_evidence(writer, &evidence);
    }
    if !metadata.capabilities.is_empty() {
        write_field_name(writer, &mut first, "capabilities");
        write_capabilities(writer, metadata.capabilities);
    }
    if let Some(auth) = metadata.auth {
        write_field_name(writer, &mut first, "auth");
        write_auth(writer, &auth);
    }
    if let Some(config) = metadata.config {
        write_field_name(writer, &mut first, "config");
        write_config_metadata(writer, &config);
    }
    writer.push_byte(b'}');
}

const fn write_field_name<const N: usize>(
    writer: &mut JsonWriter<N>,
    first: &mut bool,
    name: &str,
) {
    if *first {
        *first = false;
    } else {
        writer.push_byte(b',');
    }
    writer.push_json_str(name);
    writer.push_byte(b':');
}

const fn write_str_field<const N: usize>(
    writer: &mut JsonWriter<N>,
    first: &mut bool,
    name: &str,
    value: &str,
) {
    write_field_name(writer, first, name);
    writer.push_json_str(value);
}

const fn write_build_evidence<const N: usize>(writer: &mut JsonWriter<N>, value: &BuildEvidence) {
    writer.push_byte(b'{');
    let mut first = true;
    write_str_field(writer, &mut first, "wit", value.wit);
    write_str_field(writer, &mut first, "sdk", value.sdk);
    writer.push_byte(b'}');
}

const fn write_capabilities<const N: usize>(writer: &mut JsonWriter<N>, values: &[Need]) {
    writer.push_byte(b'[');
    let mut i = 0;
    while i < values.len() {
        if i > 0 {
            writer.push_byte(b',');
        }
        write_need(writer, &values[i]);
        i += 1;
    }
    writer.push_byte(b']');
}

const fn write_need<const N: usize>(writer: &mut JsonWriter<N>, value: &Need) {
    match value {
        Need::Domain {
            value,
            why,
            dynamic,
        } => write_scalar_need(writer, "domain", value, why, *dynamic),
        Need::GitRepo {
            value,
            why,
            dynamic,
        } => write_scalar_need(writer, "gitRepo", value, why, *dynamic),
        Need::UnixSocket {
            value,
            why,
            dynamic,
        } => write_scalar_need(writer, "unixSocket", value, why, *dynamic),
        Need::MemoryMb {
            value,
            why,
            dynamic,
        } => write_number_need(writer, "memoryMb", *value as u64, why, *dynamic),
        Need::PreopenedPath {
            host,
            guest,
            why,
            dynamic,
        } => {
            writer.push_byte(b'{');
            let mut first = true;
            write_str_field(writer, &mut first, "kind", "preopenedPath");
            write_field_name(writer, &mut first, "value");
            write_preopened_path(writer, host, guest);
            write_str_field(writer, &mut first, "why", why);
            write_field_name(writer, &mut first, "dynamic");
            writer.push_bool(*dynamic);
            writer.push_byte(b'}');
        },
    }
}

const fn write_scalar_need<const N: usize>(
    writer: &mut JsonWriter<N>,
    kind: &str,
    value: &str,
    why: &str,
    dynamic: bool,
) {
    writer.push_byte(b'{');
    let mut first = true;
    write_str_field(writer, &mut first, "kind", kind);
    write_str_field(writer, &mut first, "value", value);
    write_str_field(writer, &mut first, "why", why);
    write_field_name(writer, &mut first, "dynamic");
    writer.push_bool(dynamic);
    writer.push_byte(b'}');
}

const fn write_number_need<const N: usize>(
    writer: &mut JsonWriter<N>,
    kind: &str,
    value: u64,
    why: &str,
    dynamic: bool,
) {
    writer.push_byte(b'{');
    let mut first = true;
    write_str_field(writer, &mut first, "kind", kind);
    write_field_name(writer, &mut first, "value");
    writer.push_u64(value);
    write_str_field(writer, &mut first, "why", why);
    write_field_name(writer, &mut first, "dynamic");
    writer.push_bool(dynamic);
    writer.push_byte(b'}');
}

const fn write_preopened_path<const N: usize>(writer: &mut JsonWriter<N>, host: &str, guest: &str) {
    writer.push_byte(b'{');
    let mut first = true;
    write_str_field(writer, &mut first, "host", host);
    write_str_field(writer, &mut first, "guest", guest);
    writer.push_byte(b'}');
}

const fn write_config_metadata<const N: usize>(
    writer: &mut JsonWriter<N>,
    config: &ConfigMetadata,
) {
    writer.push_byte(b'{');
    let mut first = true;
    write_field_name(writer, &mut first, "fields");
    write_config_fields(writer, config.fields);
    writer.push_byte(b'}');
}

const fn write_config_fields<const N: usize>(writer: &mut JsonWriter<N>, fields: &[ConfigField]) {
    writer.push_byte(b'[');
    let mut i = 0;
    while i < fields.len() {
        if i > 0 {
            writer.push_byte(b',');
        }
        write_config_field(writer, &fields[i]);
        i += 1;
    }
    writer.push_byte(b']');
}

const fn write_config_field<const N: usize>(writer: &mut JsonWriter<N>, field: &ConfigField) {
    writer.push_byte(b'{');
    let mut first = true;
    write_str_field(writer, &mut first, "name", field.name);
    write_field_name(writer, &mut first, "type");
    write_config_type(writer, &field.value_type);
    if field.required {
        write_field_name(writer, &mut first, "required");
        writer.push_bool(true);
    }
    if let Some(default) = field.default {
        write_field_name(writer, &mut first, "default");
        write_default_value(writer, &default);
    }
    if let Some(description) = field.description {
        write_str_field(writer, &mut first, "description", description);
    }
    if let Some(binding) = field.binding {
        write_field_name(writer, &mut first, "binding");
        write_host_resource_binding(writer, binding);
    }
    writer.push_byte(b'}');
}

const fn write_config_type<const N: usize>(writer: &mut JsonWriter<N>, value: &ConfigType) {
    match value {
        ConfigType::String => write_config_kind(writer, "string"),
        ConfigType::Boolean => write_config_kind(writer, "boolean"),
        ConfigType::Integer => write_config_kind(writer, "integer"),
        ConfigType::Array(items) => {
            writer.push_byte(b'{');
            let mut first = true;
            write_str_field(writer, &mut first, "kind", "array");
            write_field_name(writer, &mut first, "items");
            write_config_type(writer, items);
            writer.push_byte(b'}');
        },
        ConfigType::Map(values) => {
            writer.push_byte(b'{');
            let mut first = true;
            write_str_field(writer, &mut first, "kind", "map");
            write_field_name(writer, &mut first, "values");
            write_config_type(writer, values);
            writer.push_byte(b'}');
        },
        ConfigType::Object(fields) => {
            writer.push_byte(b'{');
            let mut first = true;
            write_str_field(writer, &mut first, "kind", "object");
            write_field_name(writer, &mut first, "fields");
            write_config_fields(writer, fields);
            writer.push_byte(b'}');
        },
    }
}

const fn write_config_kind<const N: usize>(writer: &mut JsonWriter<N>, kind: &str) {
    writer.push_byte(b'{');
    let mut first = true;
    write_str_field(writer, &mut first, "kind", kind);
    writer.push_byte(b'}');
}

const fn write_default_value<const N: usize>(writer: &mut JsonWriter<N>, default: &DefaultValue) {
    match default {
        DefaultValue::String(value) => writer.push_json_str(value),
        DefaultValue::Boolean(value) => writer.push_bool(*value),
        DefaultValue::Integer(value) => writer.push_i64(*value),
    }
}

const fn write_host_resource_binding<const N: usize>(
    writer: &mut JsonWriter<N>,
    binding: HostResourceBinding,
) {
    match binding {
        HostResourceBinding::File => {
            writer.push_byte(b'{');
            let mut first = true;
            write_str_field(writer, &mut first, "kind", "file");
            writer.push_byte(b'}');
        },
        HostResourceBinding::Socket => {
            writer.push_byte(b'{');
            let mut first = true;
            write_str_field(writer, &mut first, "kind", "socket");
            writer.push_byte(b'}');
        },
    }
}

const fn write_auth<const N: usize>(writer: &mut JsonWriter<N>, auth: &Auth) {
    writer.push_byte(b'{');
    let mut first = true;
    write_field_name(writer, &mut first, "inject");
    write_inject(writer, auth);
    write_str_field(writer, &mut first, "default", auth.default);
    write_field_name(writer, &mut first, "schemes");
    write_schemes(writer, auth.schemes);
    writer.push_byte(b'}');
}

const fn write_inject<const N: usize>(writer: &mut JsonWriter<N>, auth: &Auth) {
    writer.push_byte(b'{');
    let mut first = true;
    write_field_name(writer, &mut first, "domains");
    write_str_array(writer, auth.inject.domains);
    write_str_field(writer, &mut first, "header", auth.inject.header);
    write_str_field(writer, &mut first, "prefix", auth.inject.prefix);
    writer.push_byte(b'}');
}

const fn write_schemes<const N: usize>(writer: &mut JsonWriter<N>, schemes: &[SchemeEntry]) {
    writer.push_byte(b'{');
    let mut first = true;
    let mut i = 0;
    while i < schemes.len() {
        write_field_name(writer, &mut first, schemes[i].0);
        write_scheme(writer, &schemes[i].1);
        i += 1;
    }
    writer.push_byte(b'}');
}

const fn write_scheme<const N: usize>(writer: &mut JsonWriter<N>, scheme: &Scheme) {
    match scheme {
        Scheme::StaticToken(token) => write_static_token(writer, token),
        Scheme::Oauth(oauth) => write_oauth(writer, oauth),
    }
}

const fn write_static_token<const N: usize>(writer: &mut JsonWriter<N>, token: &StaticToken) {
    writer.push_byte(b'{');
    let mut first = true;
    write_str_field(writer, &mut first, "type", "staticToken");
    write_str_field(writer, &mut first, "description", token.description);
    if let Some(url) = token.creation_url {
        write_str_field(writer, &mut first, "creationUrl", url);
    }
    if let Some(validation) = token.validation {
        write_field_name(writer, &mut first, "validation");
        write_validation(writer, &validation);
    }
    if let Some(summary) = token.summary {
        write_str_field(writer, &mut first, "summary", summary);
    }
    if !token.setup.is_empty() {
        write_field_name(writer, &mut first, "setup");
        write_str_array(writer, token.setup);
    }
    if let Some(docs_url) = token.docs_url {
        write_str_field(writer, &mut first, "docsUrl", docs_url);
    }
    writer.push_byte(b'}');
}

const fn write_oauth<const N: usize>(writer: &mut JsonWriter<N>, oauth: &OAuth) {
    writer.push_byte(b'{');
    let mut first = true;
    write_str_field(writer, &mut first, "type", "oauth");
    write_str_field(writer, &mut first, "displayName", oauth.display_name);
    if let Some(client_id) = oauth.client_id {
        write_str_field(writer, &mut first, "clientId", client_id);
    }
    if !oauth.scopes.is_empty() {
        write_field_name(writer, &mut first, "scopes");
        write_str_array(writer, oauth.scopes);
    }
    write_field_name(writer, &mut first, "flow");
    write_flow(writer, &oauth.flow);
    if let Some(summary) = oauth.summary {
        write_str_field(writer, &mut first, "summary", summary);
    }
    if !oauth.setup.is_empty() {
        write_field_name(writer, &mut first, "setup");
        write_str_array(writer, oauth.setup);
    }
    if let Some(docs_url) = oauth.docs_url {
        write_str_field(writer, &mut first, "docsUrl", docs_url);
    }
    writer.push_byte(b'}');
}

const fn write_flow<const N: usize>(writer: &mut JsonWriter<N>, flow: &Flow) {
    match flow {
        Flow::DeviceCode {
            authorization_endpoint,
            device_authorization_endpoint,
            token_endpoint,
        } => {
            writer.push_byte(b'{');
            let mut first = true;
            write_str_field(writer, &mut first, "kind", "deviceCode");
            write_str_field(
                writer,
                &mut first,
                "authorizationEndpoint",
                authorization_endpoint,
            );
            write_str_field(
                writer,
                &mut first,
                "deviceAuthorizationEndpoint",
                device_authorization_endpoint,
            );
            write_str_field(writer, &mut first, "tokenEndpoint", token_endpoint);
            writer.push_byte(b'}');
        },
        Flow::PkceLoopback {
            authorization_endpoint,
            token_endpoint,
            redirect_uri_template,
        } => {
            writer.push_byte(b'{');
            let mut first = true;
            write_str_field(writer, &mut first, "kind", "pkceLoopback");
            write_str_field(
                writer,
                &mut first,
                "authorizationEndpoint",
                authorization_endpoint,
            );
            write_str_field(writer, &mut first, "tokenEndpoint", token_endpoint);
            write_str_field(
                writer,
                &mut first,
                "redirectUriTemplate",
                redirect_uri_template,
            );
            writer.push_byte(b'}');
        },
        Flow::ClientSideToken {
            authorization_endpoint,
            token_endpoint,
            redirect_uri_template,
        } => {
            writer.push_byte(b'{');
            let mut first = true;
            write_str_field(writer, &mut first, "kind", "clientSideToken");
            write_str_field(
                writer,
                &mut first,
                "authorizationEndpoint",
                authorization_endpoint,
            );
            write_str_field(writer, &mut first, "tokenEndpoint", token_endpoint);
            write_str_field(
                writer,
                &mut first,
                "redirectUriTemplate",
                redirect_uri_template,
            );
            writer.push_byte(b'}');
        },
    }
}

const fn write_validation<const N: usize>(writer: &mut JsonWriter<N>, validation: &Validation) {
    writer.push_byte(b'{');
    let mut first = true;
    write_str_field(writer, &mut first, "method", validation.method);
    write_str_field(writer, &mut first, "url", validation.url);
    if let Some(body) = validation.body {
        write_str_field(writer, &mut first, "body", body);
    }
    write_field_name(writer, &mut first, "expectStatus");
    writer.push_u64(validation.expect_status as u64);
    if let Some(pointer) = validation.json_pointer {
        write_str_field(writer, &mut first, "jsonPointer", pointer);
    }
    if !validation.extract.is_empty() {
        write_field_name(writer, &mut first, "extract");
        write_extract(writer, validation.extract);
    }
    writer.push_byte(b'}');
}

const fn write_extract<const N: usize>(writer: &mut JsonWriter<N>, extract: &[Extract]) {
    writer.push_byte(b'{');
    let mut first = true;
    let mut i = 0;
    while i < extract.len() {
        write_str_field(writer, &mut first, extract[i].key, extract[i].pointer);
        i += 1;
    }
    writer.push_byte(b'}');
}

const fn write_str_array<const N: usize>(writer: &mut JsonWriter<N>, values: &[&str]) {
    writer.push_byte(b'[');
    let mut i = 0;
    while i < values.len() {
        if i > 0 {
            writer.push_byte(b',');
        }
        writer.push_json_str(values[i]);
        i += 1;
    }
    writer.push_byte(b']');
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Extract;

    #[test]
    fn static_metadata_json_matches_expected_dialect() {
        const AUTH: Auth = Auth::new(
            &["api.example.com"],
            "pat",
            &[(
                "pat",
                Scheme::StaticToken(
                    StaticToken::new("Example token")
                        .validation(
                            Validation::post(
                                "https://api.example.com/graphql",
                                "{\"query\":\"me\"}",
                            )
                            .extract(&[Extract::new("identity", "/data/me/id")]),
                        )
                        .summary("Paste a token."),
                ),
            )],
        );
        const CONFIG: ConfigMetadata = ConfigMetadata {
            fields: &[ConfigField {
                name: "endpoint",
                value_type: ConfigType::String,
                required: false,
                default: Some(DefaultValue::String("unix:///tmp/example.sock")),
                description: Some("Socket path."),
                binding: Some(HostResourceBinding::Socket),
            }],
        };
        const METADATA: Metadata = Metadata::new("example")
            .display_name("Example")
            .provider("omnifs_provider_example.wasm")
            .mount("example")
            .version("0.1.0")
            .build_evidence(BuildEvidence::current("0.1.0"))
            .capabilities(&[
                Need::domain("api.example.com", "Fetch API data."),
                Need::memory_mb(16, "Small heap."),
            ])
            .auth(AUTH)
            .config(Some(CONFIG));
        static BYTES: [u8; METADATA_JSON_CAPACITY] = METADATA.json_bytes();

        let value: serde_json::Value = serde_json::from_slice(&BYTES).unwrap();
        assert_eq!(value["id"], "example");
        assert_eq!(value["displayName"], "Example");
        assert_eq!(value["capabilities"][0]["kind"], "domain");
        assert_eq!(value["auth"]["schemes"]["pat"]["type"], "staticToken");
        assert_eq!(
            value["auth"]["schemes"]["pat"]["validation"]["body"],
            "{\"query\":\"me\"}"
        );
        assert_eq!(value["config"]["fields"][0]["binding"]["kind"], "socket");
    }
}
