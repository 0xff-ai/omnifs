//! Derives `omnifs_core::AuthRuntimeFingerprint` from the provider's pinned
//! auth scheme and a credential's client overrides, so a generation build can
//! detect when a stored credential no longer matches the provider it was
//! issued against.

use anyhow::Context as _;
use omnifs_api::CredentialClientOverrides;
use omnifs_auth::AuthScheme;
use omnifs_core::{AuthRuntimeFingerprint, ProviderId};
use serde::Serialize;

const AUTH_FINGERPRINT_DOMAIN: &str = "omnifs auth runtime fingerprint v1";

#[derive(Serialize)]
struct FingerprintInput<'a> {
    provider: &'a [u8; 32],
    scheme: &'a AuthScheme,
    overrides: &'a CredentialClientOverrides,
}

pub(crate) fn auth_fingerprint(
    provider: ProviderId,
    scheme: &AuthScheme,
    overrides: &CredentialClientOverrides,
) -> anyhow::Result<AuthRuntimeFingerprint> {
    let input = postcard::to_allocvec(&FingerprintInput {
        provider: provider.as_bytes(),
        scheme,
        overrides,
    })
    .context("encode auth runtime fingerprint")?;
    let mut hasher = blake3::Hasher::new_derive_key(AUTH_FINGERPRINT_DOMAIN);
    hasher.update(&input);
    Ok(AuthRuntimeFingerprint::from_digest(
        *hasher.finalize().as_bytes(),
    ))
}
