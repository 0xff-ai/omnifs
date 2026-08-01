//! Pure credential validation and durable document encoding.
//!
//! This module can read retained provider metadata and encode credential
//! material, but it has no provider runtime, serving generation, or network
//! dependency. Resource acknowledgement paths use it before the state writer.

use crate::auth_fingerprint::auth_fingerprint;
use crate::credential_codec::{CredentialPayload, encode_payload};
use anyhow::Context as _;
use omnifs_api::{
    CredentialClientOverrides, CredentialMaterial, CredentialSubmission, SecretBytes,
};
use omnifs_auth::{AuthKind, AuthScheme, CredentialId, OAuthRequest, OAuthRuntimeOverrides};
use omnifs_state::{CredentialDocument, SecretMaterial, StateStore, StoredProvider};
use secrecy::SecretString;

pub(crate) async fn prepare_credential_document(
    state: &StateStore,
    submission: CredentialSubmission,
) -> anyhow::Result<CredentialDocument> {
    let provider = state
        .load_provider(submission.provider)
        .await?
        .with_context(|| format!("provider {} is not retained", submission.provider))?;
    let id = CredentialId::new(
        provider.reference.meta.name.to_string(),
        submission.scheme.clone(),
        submission.account_label.clone(),
    )?;
    let payload = CredentialPayload {
        material: submission.material,
        overrides: submission.overrides,
    };
    let fingerprint = auth_fingerprint(
        submission.provider,
        provider
            .manifest
            .auth
            .as_ref()
            .and_then(|manifest| manifest.scheme(&submission.scheme))
            .with_context(|| format!("provider declares no auth scheme `{}`", submission.scheme))?,
        &payload.overrides,
    )?;
    let kind = validate_payload(&provider, &submission.scheme, &payload)?;
    let scopes = material_scopes(&payload.material);
    let material = encode_payload(&payload)?;
    Ok(CredentialDocument {
        id,
        provider: submission.provider,
        kind,
        auth_fingerprint: fingerprint,
        scopes,
        material: SecretMaterial::new(material),
    })
}

fn validate_payload(
    provider: &StoredProvider,
    scheme_key: &str,
    payload: &CredentialPayload,
) -> anyhow::Result<AuthKind> {
    let scheme = provider
        .manifest
        .auth
        .as_ref()
        .and_then(|manifest| manifest.scheme(scheme_key))
        .with_context(|| format!("provider declares no auth scheme `{scheme_key}`"))?;
    let kind = classify_material(&payload.material);
    match (scheme, kind) {
        (AuthScheme::StaticToken(_), AuthKind::StaticToken) => {
            anyhow::ensure!(
                no_overrides(&payload.overrides),
                "static-token credentials do not accept OAuth overrides"
            );
        },
        (AuthScheme::Oauth(scheme), AuthKind::OAuth) => {
            OAuthRequest::from_runtime(scheme.clone(), runtime_overrides(&payload.overrides)?)?;
        },
        _ => anyhow::bail!("credential material does not match provider auth scheme"),
    }
    Ok(kind)
}

fn classify_material(material: &CredentialMaterial) -> AuthKind {
    match material {
        CredentialMaterial::StaticToken { .. } => AuthKind::StaticToken,
        CredentialMaterial::OAuth { .. } => AuthKind::OAuth,
    }
}

pub(crate) fn material_scopes(material: &CredentialMaterial) -> Vec<String> {
    match material {
        CredentialMaterial::StaticToken { .. } => Vec::new(),
        CredentialMaterial::OAuth { scopes, .. } => scopes.clone(),
    }
}

pub(crate) fn runtime_overrides(
    overrides: &CredentialClientOverrides,
) -> anyhow::Result<OAuthRuntimeOverrides> {
    Ok(OAuthRuntimeOverrides {
        scopes: overrides.scopes.clone(),
        redirect_uri: overrides.redirect_uri.clone(),
        client_id: overrides.client_id.clone(),
        client_secret: overrides
            .client_secret
            .as_ref()
            .map(secret_string)
            .transpose()?,
    })
}

fn secret_string(secret: &SecretBytes) -> anyhow::Result<SecretString> {
    Ok(SecretString::from(
        std::str::from_utf8(secret.expose())
            .context("credential token is not UTF-8")?
            .to_owned(),
    ))
}

fn no_overrides(overrides: &CredentialClientOverrides) -> bool {
    overrides.client_id.is_none()
        && overrides.client_secret.is_none()
        && overrides.redirect_uri.is_none()
        && overrides.scopes.is_none()
}
