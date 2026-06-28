//! WASM custom-section IO and JSON validation for provider metadata.
//!
//! Owns wasmparser scanning for omnifs custom sections, decode entrypoints, and
//! jsonschema validators shared by file and section readers. Route/manifest
//! domain types live in sibling modules; this file stays at the IO boundary.

use crate::manifest::ProviderManifest;
use schemars::{Schema, schema_for};
use std::ops::Range;
use std::sync::OnceLock;
use wasmparser::{Parser, Payload};

pub const MANIFEST_SECTION_NAME: &str = "omnifs.provider-manifest.v1";
pub const PROVIDER_METADATA_SECTION_NAME: &str = "omnifs.provider-metadata.v1";

/// Read all `omnifs.provider-manifest.v1` custom-section bodies from a
/// wasm component's bytes, concatenating them in order. Recurses into
/// nested module/component sections so providers built as components
/// surface their guest module's section.
pub fn read_manifest_section(bytes: &[u8]) -> Result<Vec<u8>, ManifestSectionError> {
    let mut out = Vec::new();
    visit_custom_sections(
        bytes,
        |name, data| {
            if name == MANIFEST_SECTION_NAME {
                out.extend_from_slice(data);
            }
            Ok(())
        },
        |offset| ManifestSectionError::Truncated { offset },
    )?;
    Ok(out)
}

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

#[cfg(test)]
pub(crate) fn wasm_with_provider_metadata(wasm: &[u8], metadata_json: &[u8]) -> Vec<u8> {
    let mut out = wasm.to_vec();
    append_custom_section(&mut out, PROVIDER_METADATA_SECTION_NAME, metadata_json);
    out
}

#[cfg(test)]
fn append_custom_section(wasm: &mut Vec<u8>, name: &str, data: &[u8]) {
    let mut body = Vec::with_capacity(5 + name.len() + data.len());
    write_uleb128(&mut body, name.len() as u64);
    body.extend_from_slice(name.as_bytes());
    body.extend_from_slice(data);
    wasm.push(0x00);
    write_uleb128(wasm, body.len() as u64);
    wasm.extend_from_slice(&body);
}

#[cfg(test)]
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

pub(crate) fn is_hostname_only(domain: &str) -> bool {
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

#[derive(Debug, thiserror::Error)]
pub enum ManifestSectionError {
    #[error("parsing wasm: {0}")]
    Parse(#[from] wasmparser::BinaryReaderError),
    #[error("unexpected end of wasm data at offset {offset}")]
    Truncated { offset: usize },
}
