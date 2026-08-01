//! Byte encoding for `omnifs_state::SecretMaterial`: the versioned prefix and
//! postcard payload that carry a credential's material and client overrides
//! between the daemon and durable storage.

use anyhow::Context as _;
use omnifs_api::{CredentialClientOverrides, CredentialMaterial, SecretBytes};
use omnifs_auth::{AuthKind, CredentialEntry};
use secrecy::ExposeSecret as _;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

const CREDENTIAL_PAYLOAD_PREFIX: &[u8] = b"omnifs.credential.v1\0";

#[derive(Serialize, Deserialize)]
pub(crate) struct CredentialPayload {
    pub(crate) material: CredentialMaterial,
    pub(crate) overrides: CredentialClientOverrides,
}

#[derive(Serialize)]
struct RefreshedCredentialPayload<'a> {
    material: CredentialMaterial,
    overrides: &'a CredentialClientOverrides,
}

pub(crate) fn encode_payload(payload: &CredentialPayload) -> anyhow::Result<Vec<u8>> {
    encode_serialized_payload(payload)
}

pub(crate) fn encode_refreshed_payload(
    entry: &CredentialEntry,
    overrides: &CredentialClientOverrides,
) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(
        entry.kind() == AuthKind::OAuth,
        "only OAuth credentials can refresh"
    );
    // Zeroizing here only protects these transient copies; `SecretBytes`
    // itself does not zeroize on drop, and the final serialized payload
    // handed back is the persisted plaintext blob by design.
    let access_token = Zeroizing::new(entry.access_token().expose_secret().as_bytes().to_vec());
    let refresh_token = entry
        .refresh_token()
        .map(|token| Zeroizing::new(token.expose_secret().as_bytes().to_vec()));
    let material = CredentialMaterial::OAuth {
        access_token: SecretBytes::new(access_token.to_vec()),
        refresh_token: refresh_token.map(|token| SecretBytes::new(token.to_vec())),
        expires_at_unix: entry.expires_at().map(time::OffsetDateTime::unix_timestamp),
        token_type: entry.token_type().to_owned(),
        scopes: entry.scopes().to_vec(),
        upstream_identity: entry.upstream_identity().map(str::to_owned),
    };
    encode_serialized_payload(&RefreshedCredentialPayload {
        material,
        overrides,
    })
}

fn encode_serialized_payload(payload: &impl Serialize) -> anyhow::Result<Vec<u8>> {
    let encoded =
        Zeroizing::new(postcard::to_allocvec(payload).context("encode credential runtime")?);
    let mut bytes = Vec::with_capacity(CREDENTIAL_PAYLOAD_PREFIX.len() + encoded.len());
    bytes.extend_from_slice(CREDENTIAL_PAYLOAD_PREFIX);
    bytes.extend_from_slice(&encoded);
    Ok(bytes)
}

pub(crate) fn decode_payload(bytes: &[u8]) -> anyhow::Result<CredentialPayload> {
    let payload = bytes
        .strip_prefix(CREDENTIAL_PAYLOAD_PREFIX)
        .context("credential material has an unknown encoding")?;
    postcard::from_bytes(payload).context("decode credential runtime")
}
