//! WASM custom-section IO and JSON validation for provider metadata.
//!
//! Owns wasmparser scanning, metadata decode and embedding, and the
//! jsonschema validator shared by metadata readers. This file stays at the IO
//! boundary; provider metadata domain types live in sibling modules.

use crate::provider::manifest::ProviderManifest;
use schemars::{Schema, schema_for};
use std::ops::Range;
use std::sync::OnceLock;
use wasmparser::{Parser, Payload};

pub const PROVIDER_METADATA_SECTION_NAME: &str = "omnifs.provider-metadata.v1";

/// Read and decode the `omnifs.provider-metadata.v1` custom-section body
/// from a wasm component's bytes. Recurses into nested module/component
/// sections so providers built as components surface their guest module's
/// section.
pub fn read_provider_metadata_section(
    bytes: &[u8],
) -> Result<Option<ProviderManifest>, ProviderMetadataError> {
    let mut section = None;
    visit_custom_sections(
        bytes,
        |name, data| {
            if name == PROVIDER_METADATA_SECTION_NAME && section.replace(data.to_vec()).is_some() {
                return Err(ProviderMetadataError::DuplicateSection);
            }
            Ok(())
        },
        |offset| ProviderMetadataError::Truncated { offset },
    )?;

    let Some(section) = section else {
        return Ok(None);
    };
    ProviderManifest::from_bytes(&section).map(Some)
}

fn visit_custom_sections<E>(
    bytes: &[u8],
    mut on_custom: impl FnMut(&str, &[u8]) -> Result<(), E>,
    truncated: impl Fn(usize) -> E,
) -> Result<(), E>
where
    E: From<wasmparser::BinaryReaderError>,
{
    let mut work: Vec<(Parser, Range<usize>)> = vec![(Parser::new(0), 0..bytes.len())];

    while let Some((mut parser, range)) = work.pop() {
        let mut offset = range.start;
        while offset < range.end {
            let input = &bytes[offset..range.end];
            match parser.parse(input, true)? {
                wasmparser::Chunk::NeedMoreData(_) => {
                    return Err(truncated(offset));
                },
                wasmparser::Chunk::Parsed { consumed, payload } => {
                    offset += consumed;
                    match payload {
                        Payload::CustomSection(reader) => on_custom(reader.name(), reader.data())?,
                        Payload::ModuleSection {
                            parser: sub,
                            unchecked_range,
                            ..
                        }
                        | Payload::ComponentSection {
                            parser: sub,
                            unchecked_range,
                            ..
                        } => {
                            offset = offset.max(unchecked_range.end);
                            work.push((sub, unchecked_range));
                        },
                        Payload::End(_) => break,
                        _ => {},
                    }
                },
            }
        }
    }
    Ok(())
}

/// Embed `metadata_json` as the `omnifs.provider-metadata.v1` custom section,
/// returning the rewritten component bytes. Any existing top-level section with
/// that name is replaced, so re-embedding is idempotent (the reader rejects
/// duplicates). The build-time harvester serializes a provider's `Metadata`
/// const with `serde_json` and injects the result here; the host reads it back
/// pre-instantiation via [`read_provider_metadata_section`].
pub fn embed_provider_metadata_section(
    wasm: &[u8],
    metadata_json: &[u8],
) -> Result<Vec<u8>, ProviderMetadataError> {
    let mut out = Vec::with_capacity(wasm.len() + metadata_json.len() + 64);
    let mut parser = Parser::new(0);
    let mut offset = 0;
    loop {
        let start = offset;
        let (consumed, payload) = match parser.parse(&wasm[offset..], true)? {
            wasmparser::Chunk::NeedMoreData(_) => {
                return Err(ProviderMetadataError::Truncated { offset });
            },
            wasmparser::Chunk::Parsed { consumed, payload } => (consumed, payload),
        };
        offset += consumed;
        match payload {
            // Replace any existing top-level metadata section: copy nothing.
            Payload::CustomSection(reader) if reader.name() == PROVIDER_METADATA_SECTION_NAME => {},
            // A nested module/component is one opaque span at this level; copy it
            // whole and skip past its body rather than descending into it (whose
            // wasm magic would otherwise look like a stray top-level section).
            Payload::ModuleSection {
                unchecked_range, ..
            }
            | Payload::ComponentSection {
                unchecked_range, ..
            } => {
                out.extend_from_slice(&wasm[start..unchecked_range.end]);
                offset = unchecked_range.end;
            },
            Payload::End(_) => {
                out.extend_from_slice(&wasm[start..offset]);
                break;
            },
            _ => out.extend_from_slice(&wasm[start..offset]),
        }
    }
    append_custom_section(&mut out, PROVIDER_METADATA_SECTION_NAME, metadata_json);
    Ok(out)
}

#[cfg(test)]
pub(crate) fn wasm_with_provider_metadata(wasm: &[u8], metadata_json: &[u8]) -> Vec<u8> {
    let mut out = wasm.to_vec();
    append_custom_section(&mut out, PROVIDER_METADATA_SECTION_NAME, metadata_json);
    out
}

/// Append a wasm custom section (id `0`, encoded identically in the core and
/// component layers) to a finished binary.
fn append_custom_section(wasm: &mut Vec<u8>, name: &str, data: &[u8]) {
    let mut body = Vec::with_capacity(5 + name.len() + data.len());
    write_uleb128(&mut body, name.len() as u64);
    body.extend_from_slice(name.as_bytes());
    body.extend_from_slice(data);
    wasm.push(0x00);
    write_uleb128(wasm, body.len() as u64);
    wasm.extend_from_slice(&body);
}

fn write_uleb128(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            break;
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderMetadataError {
    #[error("parsing wasm: {0}")]
    Parse(#[from] wasmparser::BinaryReaderError),
    #[error("unexpected end of wasm data at offset {offset}")]
    Truncated { offset: usize },
    #[error("duplicate {PROVIDER_METADATA_SECTION_NAME} custom section")]
    DuplicateSection,
    #[error("provider metadata json decode error: {0}")]
    Json(serde_json::Error),
    #[error("provider metadata schema error: {0}")]
    Schema(String),
    #[error("invalid provider metadata: {0}")]
    Validation(String),
}

#[must_use]
pub fn provider_manifest_json() -> Schema {
    schema_for!(ProviderManifest)
}

/// Return whether `domain` is a bare hostname suitable for a capability
/// allowlist. Schemes, ports, paths, wildcards, empty labels, and labels that
/// start or end with `-` are rejected; uppercase ASCII hostnames remain valid.
pub fn is_hostname_only(domain: &str) -> bool {
    !domain.is_empty()
        && !domain.contains(['*', '/', ':', '?', '#', '@'])
        && domain.split('.').all(is_hostname_label)
}

fn is_hostname_label(label: &str) -> bool {
    !label.is_empty()
        && label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        && !label.starts_with('-')
        && !label.ends_with('-')
}

fn provider_manifest_validator() -> &'static jsonschema::Validator {
    static VALIDATOR: OnceLock<jsonschema::Validator> = OnceLock::new();
    VALIDATOR.get_or_init(|| {
        let schema =
            serde_json::to_value(provider_manifest_json()).expect("schemars output is valid JSON");
        jsonschema::validator_for(&schema).expect("derived ProviderManifest schema is well-formed")
    })
}

pub(crate) fn validate_provider_manifest(
    value: &serde_json::Value,
) -> Result<(), ProviderMetadataError> {
    provider_manifest_validator()
        .validate(value)
        .map_err(|error| ProviderMetadataError::Schema(error.to_string()))
}
