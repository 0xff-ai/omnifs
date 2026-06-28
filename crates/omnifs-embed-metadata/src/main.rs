//! Build-time metadata harvester.
//!
//! Links every provider crate as a native library, reads each provider's typed
//! `Metadata` const (via the `provider_metadata()` accessor the `#[provider]`
//! macro emits for non-wasm targets), converts it into the host's
//! [`ProviderManifest`], serializes that with `serde_json`, and injects the
//! bytes as the `omnifs.provider-metadata.v1` custom section into the
//! already-built wasm component. The host reads that section pre-instantiation;
//! this tool never instantiates a component.
//!
//! Usage: `omnifs-embed-metadata <wasm-dir>`.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use omnifs_caps::{Need as HostNeed, PreopenMode, PreopenedPath};
use omnifs_provider::{
    AuthScheme, BuildEvidence as HostBuildEvidence, ClientSideTokenConfig, ConfigField,
    ConfigMetadata, ConfigType, DeviceCodeConfig, HostResourceBinding, OAuthFlow, OauthScheme,
    PkceLoopbackConfig, ProviderAuthManifest, ProviderManifest, SchemeGuidance, StaticTokenScheme,
    TokenEndpointAuthMethod, TokenValidation, embed_provider_metadata_section,
};
use omnifs_sdk::auth::{Auth, Flow, OAuth, Scheme, StaticToken, Validation};
use omnifs_sdk::config_resource::{
    ConfigField as SdkConfigField, ConfigMetadata as SdkConfigMetadata,
    ConfigType as SdkConfigType, DefaultValue, HostResourceBinding as SdkBinding,
};
use omnifs_sdk::{Metadata, Need as SdkNeed};

type DynError = Box<dyn std::error::Error>;

/// A provider's wasm filename paired with its native metadata accessor.
type ProviderEntry = (&'static str, fn() -> Metadata);

/// The providers to embed metadata into. Adding a provider is one line here.
const PROVIDERS: &[ProviderEntry] = &[
    (
        "omnifs_provider_arxiv.wasm",
        omnifs_provider_arxiv::provider_metadata,
    ),
    (
        "omnifs_provider_db.wasm",
        omnifs_provider_db::provider_metadata,
    ),
    (
        "omnifs_provider_dns.wasm",
        omnifs_provider_dns::provider_metadata,
    ),
    (
        "omnifs_provider_docker.wasm",
        omnifs_provider_docker::provider_metadata,
    ),
    (
        "omnifs_provider_github.wasm",
        omnifs_provider_github::provider_metadata,
    ),
    (
        "omnifs_provider_kubernetes.wasm",
        omnifs_provider_kubernetes::provider_metadata,
    ),
    (
        "omnifs_provider_linear.wasm",
        omnifs_provider_linear::provider_metadata,
    ),
    (
        "omnifs_provider_oura.wasm",
        omnifs_provider_oura::provider_metadata,
    ),
    ("test_provider.wasm", test_provider::provider_metadata),
];

fn main() -> Result<(), DynError> {
    let dir = std::env::args()
        .nth(1)
        .ok_or("usage: omnifs-embed-metadata <wasm-dir>")?;
    let dir = Path::new(&dir);

    for (file, metadata) in PROVIDERS {
        let path = dir.join(file);
        let wasm =
            std::fs::read(&path).map_err(|error| format!("read {}: {error}", path.display()))?;
        let manifest = to_manifest(&metadata());
        let json = serde_json::to_vec(&manifest)
            .map_err(|error| format!("{}: serialize manifest: {error}", path.display()))?;
        let rewritten = embed_provider_metadata_section(&wasm, &json)?;
        // Validate the embedded artifact exactly as the host will read it: this
        // gates on schema + domain validation AND catches a stray duplicate
        // section (e.g. a stale nested one) before a bad wasm is written.
        omnifs_provider::read_provider_metadata_section(&rewritten)
            .map_err(|error| format!("{}: invalid embedded metadata: {error}", path.display()))?
            .ok_or_else(|| format!("{}: no metadata section after embed", path.display()))?;
        std::fs::write(&path, &rewritten)
            .map_err(|error| format!("write {}: {error}", path.display()))?;
        println!(
            "embedded metadata ({} bytes) into {}",
            json.len(),
            path.display()
        );
    }

    // Guard against the PROVIDERS registry drifting from the built wasm set: any
    // provider component in the dir we did not embed would ship metadata-less and
    // only fail at host load, with no build-time signal. Fail here instead.
    let embedded: HashSet<&str> = PROVIDERS.iter().map(|(file, _)| *file).collect();
    for entry in
        std::fs::read_dir(dir).map_err(|error| format!("scan {}: {error}", dir.display()))?
    {
        let path = entry
            .map_err(|error| format!("scan {}: {error}", dir.display()))?
            .path();
        if path.extension().is_none_or(|ext| ext != "wasm") {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let is_provider_component =
            name.starts_with("omnifs_provider_") || name == "test_provider.wasm";
        if is_provider_component && !embedded.contains(name) {
            return Err(format!(
                "{name} is a provider component but is not in the embed registry; \
                 add it to PROVIDERS in omnifs-embed-metadata"
            )
            .into());
        }
    }
    Ok(())
}

fn to_manifest(metadata: &Metadata) -> ProviderManifest {
    ProviderManifest {
        id: metadata.id.to_owned(),
        display_name: metadata.display_name.to_owned(),
        provider: metadata.provider.to_owned(),
        default_mount: metadata.default_mount.to_owned(),
        version: metadata.version.map(str::to_owned),
        build_evidence: metadata.build_evidence.map(|evidence| HostBuildEvidence {
            wit: evidence.wit.to_owned(),
            sdk: evidence.sdk.to_owned(),
        }),
        capabilities: metadata.capabilities.iter().map(to_need).collect(),
        auth: metadata.auth.map(to_auth),
        config: metadata.config.map(to_config),
    }
}

fn to_need(need: &SdkNeed) -> HostNeed {
    match *need {
        SdkNeed::Domain {
            value,
            why,
            dynamic,
        } => HostNeed::Domain {
            value: value.to_owned(),
            why: why.to_owned(),
            dynamic,
        },
        SdkNeed::GitRepo {
            value,
            why,
            dynamic,
        } => HostNeed::GitRepo {
            value: value.to_owned(),
            why: why.to_owned(),
            dynamic,
        },
        SdkNeed::UnixSocket {
            value,
            why,
            dynamic,
        } => HostNeed::UnixSocket {
            value: value.to_owned(),
            why: why.to_owned(),
            dynamic,
        },
        SdkNeed::PreopenedPath {
            host,
            guest,
            why,
            dynamic,
        } => HostNeed::PreopenedPath {
            value: PreopenedPath {
                host: host.to_owned(),
                guest: guest.to_owned(),
                mode: PreopenMode::default(),
            },
            why: why.to_owned(),
            dynamic,
        },
        SdkNeed::MemoryMb {
            value,
            why,
            dynamic,
        } => HostNeed::MemoryMb {
            value,
            why: why.to_owned(),
            dynamic,
        },
    }
}

fn to_auth(auth: Auth) -> ProviderAuthManifest {
    let mut guidance_map = BTreeMap::new();
    let mut schemes = Vec::with_capacity(auth.schemes.len());
    for (key, scheme) in auth.schemes {
        let scheme_guidance = match scheme {
            Scheme::StaticToken(token) => guidance(token.summary, token.setup, token.docs_url),
            Scheme::Oauth(oauth) => guidance(oauth.summary, oauth.setup, oauth.docs_url),
        };
        if !scheme_guidance.is_empty() {
            guidance_map.insert((*key).to_owned(), scheme_guidance);
        }
        schemes.push(to_scheme(key, scheme));
    }
    ProviderAuthManifest {
        default: auth.default.to_owned(),
        schemes,
        guidance: guidance_map,
    }
}

fn to_scheme(key: &str, scheme: &Scheme) -> AuthScheme {
    match *scheme {
        Scheme::StaticToken(token) => AuthScheme::StaticToken(to_static_token(key, token)),
        Scheme::Oauth(oauth) => AuthScheme::Oauth(to_oauth(key, oauth)),
    }
}

fn to_static_token(key: &str, token: StaticToken) -> StaticTokenScheme {
    StaticTokenScheme {
        key: key.to_owned(),
        header_name: Some(token.inject.header.to_owned()),
        value_prefix: token.inject.prefix.to_owned(),
        description: token.description.to_owned(),
        inject_domains: domains(token.inject.domains),
        creation_url: token.creation_url.map(str::to_owned),
        validation: token.validation.map(to_validation),
    }
}

fn to_oauth(key: &str, oauth: OAuth) -> OauthScheme {
    OauthScheme {
        key: key.to_owned(),
        display_name: oauth.display_name.to_owned(),
        authorization_endpoint: oauth.authorization_endpoint.to_owned(),
        token_endpoint: oauth.token_endpoint.to_owned(),
        revocation_endpoint: None,
        default_client_id: oauth.client_id.map(str::to_owned),
        default_scopes: oauth
            .scopes
            .iter()
            .map(|scope| (*scope).to_owned())
            .collect(),
        flow: to_flow(oauth.flow),
        token_endpoint_auth: TokenEndpointAuthMethod::None,
        refresh_token_rotates: matches!(oauth.flow, Flow::PkceLoopback { .. }),
        extra_authorize_params: Vec::new(),
        extra_token_params: Vec::new(),
        inject_domains: domains(oauth.inject.domains),
        inject_header_name: Some(oauth.inject.header.to_owned()),
        inject_value_prefix: oauth.inject.prefix.to_owned(),
    }
}

fn to_flow(flow: Flow) -> OAuthFlow {
    match flow {
        Flow::DeviceCode {
            device_authorization_endpoint,
        } => OAuthFlow::DeviceCode(DeviceCodeConfig {
            device_authorization_endpoint: device_authorization_endpoint.to_owned(),
        }),
        Flow::PkceLoopback {
            redirect_uri_template,
        } => OAuthFlow::PkceLoopback(PkceLoopbackConfig {
            redirect_uri_template: redirect_uri_template.to_owned(),
        }),
        Flow::ClientSideToken {
            redirect_uri_template,
        } => OAuthFlow::ClientSideToken(ClientSideTokenConfig {
            redirect_uri_template: redirect_uri_template.to_owned(),
        }),
    }
}

fn to_validation(validation: Validation) -> TokenValidation {
    TokenValidation {
        method: validation.method.to_owned(),
        url: validation.url.to_owned(),
        body: validation.body.map(str::to_owned),
        expect_status: validation.expect_status,
        json_pointer: validation.json_pointer.map(str::to_owned),
        extract: validation
            .extract
            .iter()
            .map(|entry| (entry.key.to_owned(), entry.pointer.to_owned()))
            .collect(),
    }
}

fn guidance(summary: Option<&str>, setup: &[&str], docs_url: Option<&str>) -> SchemeGuidance {
    SchemeGuidance {
        summary: summary.map(str::to_owned),
        setup_steps: setup.iter().map(|step| (*step).to_owned()).collect(),
        docs_url: docs_url.map(str::to_owned),
    }
}

fn to_config(config: SdkConfigMetadata) -> ConfigMetadata {
    ConfigMetadata {
        fields: config.fields.iter().map(to_config_field).collect(),
    }
}

fn to_config_field(field: &SdkConfigField) -> ConfigField {
    ConfigField {
        name: field.name.to_owned(),
        value_type: to_config_type(&field.value_type),
        required: field.required,
        default: field.default.map(to_default_value),
        description: field.description.map(str::to_owned),
        binding: field.binding.map(to_binding),
    }
}

fn to_config_type(value_type: &SdkConfigType) -> ConfigType {
    match value_type {
        SdkConfigType::String => ConfigType::String,
        SdkConfigType::Boolean => ConfigType::Boolean,
        SdkConfigType::Integer => ConfigType::Integer,
        SdkConfigType::Array(items) => ConfigType::Array {
            items: Box::new(to_config_type(items)),
        },
        SdkConfigType::Map(values) => ConfigType::Map {
            values: Box::new(to_config_type(values)),
        },
        SdkConfigType::Object(fields) => ConfigType::Object {
            fields: fields.iter().map(to_config_field).collect(),
        },
    }
}

fn to_default_value(default: DefaultValue) -> serde_json::Value {
    match default {
        DefaultValue::String(value) => serde_json::Value::String(value.to_owned()),
        DefaultValue::Boolean(value) => serde_json::Value::Bool(value),
        DefaultValue::Integer(value) => serde_json::Value::Number(value.into()),
    }
}

fn to_binding(binding: SdkBinding) -> HostResourceBinding {
    match binding {
        SdkBinding::File => HostResourceBinding::File {
            mode: PreopenMode::default(),
        },
        SdkBinding::Socket => HostResourceBinding::Socket,
    }
}

fn domains(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}
