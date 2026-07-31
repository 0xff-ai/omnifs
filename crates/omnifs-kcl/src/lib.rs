//! In-process KCL authoring for the omnifs resource model.
//!
//! KCL is deliberately kept at this boundary. The evaluator produces an
//! in-memory authoring value; callers convert it to the strict omnifs API
//! types before talking to the daemon.

mod evaluator;
mod source;

#[cfg(test)]
use evaluator::evaluate_sync;
pub use evaluator::{EvaluateError, EvaluatedConfig, evaluate};
pub use source::{
    AttachmentAuthoring, AuthoringConfig, AuthoringResource, LocalProviderSource,
    ProviderAuthoring, ProviderSource, SourceResolutionError, resolve_local_source,
};

use omnifs_api::{NormalizedResourceSet, ResourceDefinition};
use serde_json::Value;

/// Embedded editor schema source. The v1 evaluator accepts the documented
/// plain-root fallback because `kcl-api` has no in-process package injection
/// API; this constant keeps the schema available to a future editor/package
/// bridge without requiring a system `kcl` installation.
#[cfg(test)]
const OMNIFS_KCL_SCHEMA: &str = include_str!("../assets/omnifs.k");

/// Render a normalized resource set as deterministic KCL source.
///
/// This is presentation only. The daemon digest remains the digest of the
/// strict Rust declarations, never this text.
#[must_use]
pub fn render_config(resources: &NormalizedResourceSet) -> String {
    let mut output =
        String::from("config = {\n    apiVersion = \"omnifs.dev/v1alpha1\"\n    resources = [\n");
    for resource in resources.resources() {
        output.push_str("        ");
        render_resource(&mut output, resource);
        output.push_str(",\n");
    }
    output.push_str("    ]\n}\n");
    output
}

fn render_resource(output: &mut String, resource: &ResourceDefinition) {
    match resource {
        ResourceDefinition::Provider(value) => {
            output.push_str("{kind = \"Provider\", spec = {name = ");
            output.push_str(&json_string(value.name.as_str()));
            output.push_str(", source = {digest = ");
            output.push_str(&json_string(&value.artifact.to_string()));
            output.push_str("}}}");
        },
        ResourceDefinition::Credential(value) => {
            output.push_str("{kind = \"Credential\", spec = {name = ");
            output.push_str(&json_string(value.name.as_str()));
            output.push_str(", provider = ");
            output.push_str(&json_string(value.provider.as_str()));
            output.push_str(", scheme = ");
            output.push_str(&json_string(&value.scheme));
            output.push_str(", account = ");
            output.push_str(&json_string(&value.account));
            output.push_str("}}");
        },
        ResourceDefinition::Mount(value) => {
            output.push_str("{kind = \"Mount\", spec = {name = ");
            output.push_str(&json_string(value.name.as_str()));
            output.push_str(", provider = ");
            output.push_str(&json_string(value.provider.as_str()));
            output.push_str(", credential = ");
            if let Some(credential) = &value.credential {
                output.push_str(&json_string(credential.as_str()));
            } else {
                output.push_str("None");
            }
            output.push_str(", config = ");
            render_json_value(output, &value.config);
            output.push_str(", limits = ");
            if let Some(limits) = &value.limits {
                render_limits(output, limits);
            } else {
                output.push_str("None");
            }
            output.push_str("}}");
        },
        ResourceDefinition::Attachment(value) => {
            let spec = &value.spec;
            output.push_str("{kind = \"Attachment\", spec = {name = ");
            output.push_str(&json_string(value.name.as_str()));
            output.push_str(", protocol = ");
            output.push_str(&json_string(spec.protocol().as_str()));
            output.push_str(", runtime = ");
            output.push_str(&json_string(spec.runtime().as_str()));
            output.push_str(", location = ");
            output.push_str(&json_string(&spec.location().to_string_lossy()));
            output.push_str(", dockerImage = ");
            if let Some(image) = spec.docker_image() {
                output.push_str(&json_string(image));
            } else {
                output.push_str("None");
            }
            output.push_str(", libkrunGuestImage = ");
            if let Some(image) = spec.libkrun_guest_image() {
                output.push_str(&json_string(image));
            } else {
                output.push_str("None");
            }
            output.push_str("}}");
        },
    }
}

fn render_limits(output: &mut String, limits: &omnifs_api::ResourceLimits) {
    output.push('{');
    let mut first = true;
    if let Some(value) = limits.max_memory_mb {
        output.push_str("maxMemoryMb = ");
        output.push_str(&value.to_string());
        first = false;
    }
    if let Some(value) = limits.max_fetch_blob_bytes {
        if !first {
            output.push_str(", ");
        }
        output.push_str("maxFetchBlobBytes = ");
        output.push_str(&value.to_string());
    }
    output.push('}');
}

fn render_json_value(output: &mut String, value: &Value) {
    match value {
        Value::Null => output.push_str("None"),
        Value::Bool(value) => output.push_str(if *value { "True" } else { "False" }),
        Value::Number(value) => output.push_str(&value.to_string()),
        Value::String(value) => output.push_str(&json_string(value)),
        Value::Array(values) => {
            output.push('[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push_str(", ");
                }
                render_json_value(output, value);
            }
            output.push(']');
        },
        Value::Object(values) => {
            output.push('{');
            for (index, (key, value)) in values.iter().enumerate() {
                if index > 0 {
                    output.push_str(", ");
                }
                if is_kcl_identifier(key) {
                    output.push_str(key);
                } else {
                    output.push_str(&json_string(key));
                }
                output.push_str(" = ");
                render_json_value(output, value);
            }
            output.push('}');
        },
    }
}

fn is_kcl_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).expect("serializing a string cannot fail")
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_api::{
        API_VERSION, AttachmentDefinition, CredentialDefinition, MountResourceDefinition,
        ProviderDefinition, ResourceDeclarations, ResourceDefinition, ResourceLimits,
    };
    use omnifs_core::{
        AttachmentProtocol, AttachmentRuntime, AttachmentSpec, ProviderId, ResourceName,
    };
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn name(value: &str) -> ResourceName {
        ResourceName::new(value).unwrap()
    }

    #[test]
    fn renders_all_resource_shapes_and_round_trips() {
        let resources = ResourceDeclarations {
            api_version: API_VERSION.into(),
            resources: vec![
                ResourceDefinition::Provider(ProviderDefinition {
                    name: name("demo"),
                    artifact: ProviderId::from_wasm_bytes(b"demo"),
                }),
                ResourceDefinition::Credential(CredentialDefinition {
                    name: name("demo-creds"),
                    provider: name("demo"),
                    scheme: "oauth".into(),
                    account: "default".into(),
                }),
                ResourceDefinition::Mount(MountResourceDefinition {
                    name: name("demo-mount"),
                    provider: name("demo"),
                    credential: Some(name("demo-creds")),
                    config: serde_json::json!({"x-y": "line\nquote\"", "enabled": true}),
                    limits: Some(ResourceLimits {
                        max_memory_mb: Some(64),
                        max_fetch_blob_bytes: Some(1024),
                    }),
                }),
                ResourceDefinition::Attachment(AttachmentDefinition {
                    name: name("demo-fs"),
                    spec: AttachmentSpec::new(
                        AttachmentProtocol::Nfs,
                        AttachmentRuntime::Host,
                        PathBuf::from("/tmp/omnifs-demo"),
                        None,
                        None,
                    )
                    .unwrap(),
                }),
            ],
        }
        .normalize()
        .unwrap();
        let rendered = render_config(&resources);
        assert!(rendered.contains("kind = \"Provider\""));
        assert!(rendered.contains("kind = \"Attachment\""));
        let dir = tempdir().unwrap();
        let file = dir.path().join("roundtrip.k");
        std::fs::write(&file, rendered).unwrap();
        let evaluated = evaluate_sync(&file).unwrap();
        let normalized = evaluated
            .config
            .into_declarations(&std::collections::BTreeMap::new())
            .unwrap()
            .normalize()
            .unwrap();
        assert_eq!(normalized, resources);
    }

    #[test]
    fn strict_normalization_rejects_duplicate_resources() {
        let declarations = ResourceDeclarations {
            api_version: API_VERSION.into(),
            resources: vec![
                ResourceDefinition::Provider(ProviderDefinition {
                    name: name("demo"),
                    artifact: ProviderId::from_wasm_bytes(b"one"),
                }),
                ResourceDefinition::Provider(ProviderDefinition {
                    name: name("demo"),
                    artifact: ProviderId::from_wasm_bytes(b"two"),
                }),
            ],
        };
        assert!(matches!(
            declarations.normalize(),
            Err(omnifs_api::ResourceDefinitionError::DuplicateKey(_))
        ));
    }

    #[test]
    fn embedded_schema_covers_each_resource_kind() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("schema.k");
        std::fs::write(
            &file,
            format!(
                r#"{OMNIFS_KCL_SCHEMA}
config = Config {{
    apiVersion = "omnifs.dev/v1alpha1"
    resources = [
        ProviderResource {{
            kind = "Provider"
            spec = ProviderSpec {{
                name = "demo"
                source = ProviderSource {{ digest = "{digest}" }}
            }}
        }}
        CredentialResource {{
            kind = "Credential"
            spec = CredentialSpec {{
                name = "demo-creds"
                provider = "demo"
                scheme = "oauth"
                account = "default"
            }}
        }}
        MountResource {{
            kind = "Mount"
            spec = MountSpec {{
                name = "demo-mount"
                provider = "demo"
                credential = "demo-creds"
                config = {{enabled = True}}
                limits = ResourceLimits {{ maxMemoryMb = 64 }}
            }}
        }}
        AttachmentResource {{
            kind = "Attachment"
            spec = AttachmentSpec {{
                name = "demo-attachment"
                protocol = "nfs"
                runtime = "host"
                location = "/tmp/omnifs-demo"
            }}
        }}
    ]
}}
"#,
                digest = ProviderId::from_wasm_bytes(b"demo"),
            ),
        )
        .unwrap();

        let evaluated = evaluate_sync(&file).unwrap();
        assert_eq!(evaluated.config.resources.len(), 4);
        assert!(matches!(
            evaluated.config.resources.as_slice(),
            [
                AuthoringResource::Provider(_),
                AuthoringResource::Credential(_),
                AuthoringResource::Mount(_),
                AuthoringResource::Attachment(_),
            ]
        ));
    }

    #[test]
    fn embedded_schema_rejects_multiple_provider_source_selectors() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("invalid-source.k");
        std::fs::write(
            &file,
            format!(
                r#"{OMNIFS_KCL_SCHEMA}
config = Config {{
    apiVersion = "omnifs.dev/v1alpha1"
    resources = [
        ProviderResource {{
            kind = "Provider"
            spec = ProviderSpec {{
                name = "demo"
                source = ProviderSource {{
                    embedded = "demo"
                    digest = "{digest}"
                }}
            }}
        }}
    ]
}}
"#,
                digest = ProviderId::from_wasm_bytes(b"demo"),
            ),
        )
        .unwrap();

        assert!(matches!(evaluate_sync(&file), Err(EvaluateError::Kcl(_))));
    }
}
