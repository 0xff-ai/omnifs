//! Assembles one durable serving generation from durable state.
//!
//! [`GenerationDraft::load_resources`] reads the durable desired resources;
//! [`GenerationDraft::prepare`] resolves every mounted provider and
//! credential, binds auth, and returns a [`GenerationBuild`] ready to
//! publish. Resource reconciliation owns the load/prepare/activate pass.

mod refresh_sink;
mod revocation;

pub(crate) use revocation::prepare_credential_revocation;

use crate::auth_fingerprint::auth_fingerprint;
use crate::credential_codec::decode_payload;
use crate::credential_document::{material_scopes, runtime_overrides};
use anyhow::Context as _;
use omnifs_api::{CredentialClientOverrides, CredentialMaterial, ResourceDefinition, SecretBytes};
use omnifs_auth::{
    AuthBinding, AuthKind, AuthScheme, CredentialEntry, CredentialId, CredentialService,
    DurableCredentialSnapshot, OAuthClient, OAuthRequest, RefreshSink,
};
use omnifs_core::{
    ActionId, AuthRuntimeFingerprint, CredentialGeneration, CredentialVersion, MountVersion,
    ProviderId, ResourceName, ResourceRevision,
};
use omnifs_engine::{
    CredentialProvenance, GenerationProvenance, HostOnline, MountBuildInput, MountBuildState,
    MountProvenance, MountTable, PreparedGeneration, ProviderBuildInput, PublishReadyGeneration,
    RuntimeMountConfig,
};
use omnifs_state::{
    CredentialRevocationFinish, CredentialState, StateStore, StoredCredential, StoredProvider,
    StoredProviderMetadata,
};
use refresh_sink::StateRefreshSink;
use secrecy::SecretString;
use std::collections::HashMap;
use std::sync::Arc;
use time::OffsetDateTime;

/// The durable serving snapshot as of `load`: every active mount and
/// credential a fresh generation should be built from. Resource apply
/// commits desired state first; `load` then re-reads the result, so the draft
/// always reflects the durable outcome rather than staging it separately.
pub(crate) struct GenerationDraft {
    revision: ResourceRevision,
    mounts: Vec<ResolvedMount>,
    credentials: Vec<CredentialRuntime>,
    pending_refreshes: Vec<PendingRefresh>,
}

pub(crate) struct ResolvedDesired {
    pub(crate) revision: ResourceRevision,
    pub(crate) mounts: Vec<ResolvedMount>,
    credentials: Vec<CredentialId>,
}

pub(crate) struct ResolvedMount {
    pub(crate) name: ResourceName,
    pub(crate) provider: omnifs_core::ProviderRef,
    pub(crate) credential: Option<CredentialId>,
    pub(crate) limits: Option<omnifs_api::ResourceLimits>,
    pub(crate) config: serde_json::Value,
    pub(crate) canonical: Vec<u8>,
    pub(crate) version: MountVersion,
    pub(crate) revision: ResourceRevision,
}

impl ResolvedMount {
    fn new(
        definition: &omnifs_api::MountResourceDefinition,
        provider: omnifs_core::ProviderRef,
        credential: Option<CredentialId>,
        revision: ResourceRevision,
    ) -> anyhow::Result<Self> {
        let credential_key = credential.as_ref().map(|id| {
            (
                id.provider_name().to_owned(),
                id.scheme().to_owned(),
                id.account().to_owned(),
            )
        });
        let canonical = serde_json::to_vec(&(
            definition.name.as_str(),
            provider.id,
            provider.meta.name.as_str(),
            provider.meta.version.as_ref().map(ToString::to_string),
            credential_key,
            &definition.limits,
            &definition.config,
        ))
        .context("encode resolved mount")?;
        let mut hasher = blake3::Hasher::new_derive_key("omnifs resolved mount version v1");
        hasher.update(&canonical);
        Ok(Self {
            name: definition.name.clone(),
            provider,
            credential,
            limits: definition.limits.clone(),
            config: definition.config.clone(),
            canonical,
            version: MountVersion::from_digest(*hasher.finalize().as_bytes()),
            revision,
        })
    }
}

impl ResolvedDesired {
    pub(crate) async fn load(state: &StateStore) -> anyhow::Result<Self> {
        let desired = state.resource_snapshot().await?;
        let revision = desired.revision;
        let mut providers = HashMap::<ResourceName, StoredProviderMetadata>::new();
        for resource in desired.resources.resources() {
            let ResourceDefinition::Provider(provider) = resource else {
                continue;
            };
            let retained = state
                .load_provider_metadata(provider.artifact)
                .await?
                .with_context(|| {
                    format!(
                        "provider resource `{}` artifact {} is not retained",
                        provider.name, provider.artifact
                    )
                })?;
            providers.insert(provider.name.clone(), retained);
        }

        let mut credential_ids = HashMap::new();
        for resource in desired.resources.resources() {
            let ResourceDefinition::Credential(credential) = resource else {
                continue;
            };
            let provider = providers.get(&credential.provider).with_context(|| {
                format!(
                    "credential resource `{}` provider `{}` is absent",
                    credential.name, credential.provider
                )
            })?;
            credential_ids.insert(
                credential.name.clone(),
                CredentialId::new(
                    provider.reference.meta.name.to_string(),
                    credential.scheme.clone(),
                    credential.account.clone(),
                )?,
            );
        }

        let mut mounts = Vec::new();
        for resource in desired.resources.resources() {
            let ResourceDefinition::Mount(mount) = resource else {
                continue;
            };
            let provider = providers.get(&mount.provider).with_context(|| {
                format!(
                    "mount resource `{}` provider `{}` is absent",
                    mount.name, mount.provider
                )
            })?;
            let credential = mount
                .credential
                .as_ref()
                .map(|name| {
                    credential_ids.get(name).cloned().with_context(|| {
                        format!(
                            "mount resource `{}` credential `{name}` is absent",
                            mount.name
                        )
                    })
                })
                .transpose()?;
            mounts.push(ResolvedMount::new(
                mount,
                provider.reference.clone(),
                credential,
                revision,
            )?);
        }

        Ok(Self {
            revision,
            mounts,
            credentials: credential_ids.into_values().collect(),
        })
    }
}

pub(crate) fn empty_generation(host: &HostOnline) -> anyhow::Result<PublishReadyGeneration> {
    let table = Arc::new(MountTable::prepare_durable(host, Vec::new())?);
    Ok(PreparedGeneration::new(
        table,
        tokio::runtime::Handle::current(),
        GenerationProvenance::default(),
    )
    .activate())
}

impl GenerationDraft {
    /// Resolve the authoritative declarative resources into one immutable
    /// serving draft. Provider resource names are aliases only; mounted
    /// generations pin the exact retained artifact digest.
    pub(crate) async fn load_resources(state: &StateStore) -> anyhow::Result<Self> {
        let resolved = ResolvedDesired::load(state).await?;
        let mut credentials = Vec::new();
        let mut pending_refreshes = Vec::new();
        for id in resolved.credentials {
            let Some(stored) = state.get_credential(&id).await? else {
                continue;
            };
            collect_credential(stored, &mut credentials, &mut pending_refreshes)?;
        }

        Ok(Self {
            revision: resolved.revision,
            mounts: resolved.mounts,
            credentials,
            pending_refreshes,
        })
    }

    pub(crate) fn provenance(&self) -> GenerationProvenance {
        GenerationProvenance::new(
            self.revision,
            self.mounts
                .iter()
                .map(|mount| MountProvenance {
                    name: mount.name.clone(),
                    version: mount.version,
                })
                .collect(),
            self.credentials
                .iter()
                .map(|credential| CredentialProvenance {
                    id: credential.id.clone(),
                    version: credential.version,
                    generation: credential.generation,
                })
                .collect(),
        )
    }

    /// Resolve every mounted provider and credential, bind auth, and build a
    /// complete durable `MountTable` generation.
    ///
    /// A provider store failure fails preparation outright: it is
    /// indistinguishable in principle from any other durable-state failure
    /// and must not be reported as "this mount's provider is unretained".
    /// Likewise, a credential whose auth runtime fingerprint no longer
    /// matches its pinned provider, or any other `build_auth_binding`
    /// failure, fails preparation rather than degrading the mount to
    /// `AuthRequired`. Only a mount whose credential lookup finds nothing (or
    /// finds one bound to a different provider) degrades to
    /// `MountBuildState::AuthRequired`: that is the one case that is
    /// genuinely a missing/stale credential rather than a defect.
    pub(crate) async fn prepare(
        self,
        state: &Arc<StateStore>,
        host: &HostOnline,
    ) -> anyhow::Result<GenerationBuild> {
        let provenance = self.provenance();
        let Self {
            mounts,
            credentials: draft_credentials,
            pending_refreshes,
            ..
        } = self;

        let mut providers: HashMap<ProviderId, Option<LoadedProvider>> = HashMap::new();
        for mount in &mounts {
            let id = mount.provider.id;
            if let std::collections::hash_map::Entry::Vacant(entry) = providers.entry(id) {
                let provider = state
                    .load_provider(id)
                    .await
                    .with_context(|| format!("load provider {id}"))?;
                entry.insert(provider.map(LoadedProvider::from));
            }
        }

        let mut credentials = HashMap::new();
        let mut durable_snapshots = Vec::new();
        for runtime in draft_credentials {
            durable_snapshots.push((
                runtime.id.clone(),
                DurableCredentialSnapshot {
                    entry: runtime.entry.clone(),
                    version: runtime.version,
                },
            ));
            credentials.insert(runtime.id.clone(), runtime);
        }
        let credentials = Arc::new(credentials);
        let refresh_sink: Arc<dyn RefreshSink> = Arc::new(StateRefreshSink::new(
            Arc::clone(state),
            Arc::clone(&credentials),
        ));
        let service = Arc::new(CredentialService::new(
            durable_snapshots,
            OAuthClient::new()?,
            refresh_sink,
        ));

        let mut inputs = Vec::with_capacity(mounts.len());
        for mount in mounts {
            let provider = providers
                .get(&mount.provider.id)
                .expect("every mount provider was loaded")
                .as_ref();
            inputs.push(build_mount_input(mount, provider, &credentials, &service)?);
        }
        let table = Arc::new(MountTable::prepare_durable(host, inputs)?);
        Ok(GenerationBuild {
            generation: PreparedGeneration::new(
                table,
                tokio::runtime::Handle::current(),
                provenance,
            ),
            pending_refreshes: PendingRefreshes(pending_refreshes),
        })
    }
}

fn collect_credential(
    stored: StoredCredential,
    credentials: &mut Vec<CredentialRuntime>,
    pending_refreshes: &mut Vec<PendingRefresh>,
) -> anyhow::Result<()> {
    match stored.summary.state {
        CredentialState::Active => {},
        CredentialState::PendingRepublish => {
            pending_refreshes.push(PendingRefresh {
                id: stored.summary.id.clone(),
                version: stored.summary.version,
                generation: stored.summary.generation,
            });
        },
        CredentialState::Blocked
        | CredentialState::RevocationPending
        | CredentialState::RevocationUnknown
        | CredentialState::Deleted => return Ok(()),
    }
    credentials.push(CredentialRuntime::from_stored(stored)?);
    Ok(())
}

struct PendingRefresh {
    id: CredentialId,
    version: CredentialVersion,
    generation: CredentialGeneration,
}

/// A durable serving generation built from a [`GenerationDraft`], plus the
/// credentials awaiting activation once it publishes.
pub(crate) struct GenerationBuild {
    generation: PreparedGeneration,
    pending_refreshes: PendingRefreshes,
}

/// The named pieces of a [`GenerationBuild`], split apart at the point a
/// caller is ready to publish.
pub(crate) struct GenerationParts {
    pub(crate) ready: PublishReadyGeneration,
    pub(crate) revision: ResourceRevision,
    pub(crate) pending_refreshes: PendingRefreshes,
}

impl GenerationBuild {
    pub(crate) fn into_parts(self) -> GenerationParts {
        let revision = self.generation.provenance().revision();
        GenerationParts {
            ready: self.generation.activate(),
            revision,
            pending_refreshes: self.pending_refreshes,
        }
    }
}

/// Credentials that finished a refresh while `PendingRepublish` and now need
/// to be marked active now that the generation carrying them has published.
pub(crate) struct PendingRefreshes(Vec<PendingRefresh>);

impl PendingRefreshes {
    pub(crate) async fn activate(self, state: &StateStore) -> anyhow::Result<()> {
        for pending in self.0 {
            state
                .activate_refreshed_credential(pending.id, pending.version, pending.generation)
                .await
                .context("activate refreshed credential")?;
        }
        Ok(())
    }
}

struct CredentialRuntime {
    id: CredentialId,
    provider: ProviderId,
    kind: AuthKind,
    fingerprint: AuthRuntimeFingerprint,
    version: CredentialVersion,
    generation: CredentialGeneration,
    entry: CredentialEntry,
    overrides: Arc<CredentialClientOverrides>,
}

impl CredentialRuntime {
    fn from_stored(stored: StoredCredential) -> anyhow::Result<Self> {
        let payload = decode_payload(stored.material.expose())?;
        let entry = decode_entry(&payload.material)?;
        Ok(Self {
            id: stored.summary.id,
            provider: stored.summary.provider,
            kind: stored.summary.kind,
            fingerprint: stored.summary.auth_fingerprint,
            version: stored.summary.version,
            generation: stored.summary.generation,
            entry,
            overrides: Arc::new(payload.overrides),
        })
    }
}

struct LoadedProvider {
    reference: omnifs_core::ProviderRef,
    manifest: omnifs_provider::ProviderManifest,
    bytes: Arc<[u8]>,
}

impl From<StoredProvider> for LoadedProvider {
    fn from(provider: StoredProvider) -> Self {
        Self {
            reference: provider.reference,
            manifest: provider.manifest,
            bytes: Arc::from(provider.bytes.into_boxed_slice()),
        }
    }
}

/// Return the non-secret scopes granted by a stored OAuth credential.
///
/// Credential material stays daemon-owned. Callers receive only this narrow
/// presentation fact, never the access or refresh token.
pub(crate) fn credential_scopes(stored: &StoredCredential) -> anyhow::Result<Vec<String>> {
    if stored.summary.state == CredentialState::Deleted {
        return Ok(Vec::new());
    }
    let payload = decode_payload(stored.material.expose())?;
    Ok(material_scopes(&payload.material))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RevocationActionOutcome {
    Deleted,
    Unknown,
}

/// Finish the upstream part of a declarative credential action after the old
/// generation has drained and can no longer admit calls with the old secret.
pub(crate) async fn finish_resource_credential_revocation(
    state: &StateStore,
    credential_name: &ResourceName,
    action_id: ActionId,
) -> anyhow::Result<RevocationActionOutcome> {
    let desired = state.resource_snapshot().await?;
    let credential = desired
        .resources
        .resources()
        .iter()
        .find_map(|resource| match resource {
            ResourceDefinition::Credential(credential) if credential.name == *credential_name => {
                Some(credential)
            },
            _ => None,
        })
        .with_context(|| format!("credential resource `{credential_name}` is absent"))?;
    let provider = desired
        .resources
        .resources()
        .iter()
        .find_map(|resource| match resource {
            ResourceDefinition::Provider(provider) if provider.name == credential.provider => {
                Some(provider)
            },
            _ => None,
        })
        .with_context(|| {
            format!(
                "credential resource `{credential_name}` provider `{}` is absent",
                credential.provider
            )
        })?;
    let metadata = state
        .load_provider_metadata(provider.artifact)
        .await?
        .with_context(|| format!("provider {} is not retained", provider.artifact))?;
    let id = omnifs_auth::CredentialId::new(
        metadata.reference.meta.name.to_string(),
        credential.scheme.clone(),
        credential.account.clone(),
    )?;
    let stored = state
        .get_credential(&id)
        .await?
        .with_context(|| format!("credential resource `{credential_name}` has no material"))?;
    let scopes = credential_scopes(&stored)?;
    let revocation = prepare_credential_revocation(state, &stored).await?;
    let finish = match tokio::time::timeout(std::time::Duration::from_secs(15), revocation.revoke())
        .await
    {
        Ok(Ok(())) => CredentialRevocationFinish::Deleted,
        Ok(Err(error)) => {
            tracing::warn!(credential = %credential_name, %error, "credential revocation failed");
            CredentialRevocationFinish::Unknown
        },
        Err(_) => {
            tracing::warn!(credential = %credential_name, "credential revocation timed out");
            CredentialRevocationFinish::Unknown
        },
    };
    state
        .finish_credential_revocation(id, action_id, finish, scopes)
        .await?;
    Ok(match finish {
        CredentialRevocationFinish::Deleted => RevocationActionOutcome::Deleted,
        CredentialRevocationFinish::Unknown => RevocationActionOutcome::Unknown,
    })
}

fn build_mount_input(
    mount: ResolvedMount,
    provider: Option<&LoadedProvider>,
    credentials: &HashMap<CredentialId, CredentialRuntime>,
    service: &Arc<CredentialService>,
) -> anyhow::Result<MountBuildInput> {
    let config = RuntimeMountConfig {
        name: mount.name,
        provider: mount.provider.clone(),
        config: mount.config,
        max_fetch_blob_bytes: mount.limits.and_then(|limits| limits.max_fetch_blob_bytes),
    };
    let canonical = Arc::from(mount.canonical.into_boxed_slice());
    let Some(provider) = provider else {
        return Ok(MountBuildInput {
            config,
            canonical,
            provider: None,
            state: MountBuildState::ProviderUnavailable,
        });
    };
    let provider_input = Some(ProviderBuildInput {
        bytes: Arc::clone(&provider.bytes),
        manifest: provider.manifest.clone(),
    });
    let state = match mount.credential.as_ref() {
        None => MountBuildState::Active {
            auth: None,
            credential_generation: None,
        },
        Some(id) => {
            let bound = credentials
                .get(id)
                .filter(|credential| credential.provider == provider.reference.id);
            match bound {
                None => MountBuildState::AuthRequired,
                Some(credential) => {
                    let auth = build_auth_binding(provider, credential, service)?;
                    MountBuildState::Active {
                        auth: Some(auth),
                        credential_generation: Some(credential.generation),
                    }
                },
            }
        },
    };
    Ok(MountBuildInput {
        config,
        canonical,
        provider: provider_input,
        state,
    })
}

fn build_auth_binding(
    provider: &LoadedProvider,
    credential: &CredentialRuntime,
    service: &Arc<CredentialService>,
) -> anyhow::Result<Arc<AuthBinding>> {
    let scheme = provider
        .manifest
        .auth
        .as_ref()
        .and_then(|manifest| manifest.scheme(credential.id.scheme()))
        .context("credential scheme is absent from the pinned provider")?;
    anyhow::ensure!(
        auth_fingerprint(provider.reference.id, scheme, &credential.overrides)?
            == credential.fingerprint,
        "credential auth runtime no longer matches the pinned provider"
    );
    let binding = match (scheme, credential.kind) {
        (AuthScheme::StaticToken(scheme), AuthKind::StaticToken) => service.bind_static(
            credential.id.clone(),
            scheme.inject_domains.clone(),
            scheme
                .header_name
                .clone()
                .unwrap_or_else(|| "Authorization".to_owned()),
            scheme.value_prefix.clone(),
        )?,
        (AuthScheme::Oauth(scheme), AuthKind::OAuth) => {
            let request = OAuthRequest::from_runtime(
                scheme.clone(),
                runtime_overrides(&credential.overrides)?,
            )?;
            service.bind_oauth(
                credential.id.clone(),
                request,
                scheme.inject_domains.clone(),
                scheme
                    .inject_header_name
                    .clone()
                    .unwrap_or_else(|| "Authorization".to_owned()),
                scheme.inject_value_prefix.clone(),
            )?
        },
        _ => anyhow::bail!("credential kind does not match its provider scheme"),
    };
    Ok(Arc::new(binding))
}

fn decode_entry(material: &CredentialMaterial) -> anyhow::Result<CredentialEntry> {
    Ok(match material {
        CredentialMaterial::StaticToken { token } => {
            CredentialEntry::static_token(secret_string(token)?)
        },
        CredentialMaterial::OAuth {
            access_token,
            refresh_token,
            expires_at_unix,
            token_type,
            scopes,
            upstream_identity,
        } => {
            let expires_at = expires_at_unix
                .map(OffsetDateTime::from_unix_timestamp)
                .transpose()
                .context("credential expiry is outside the supported timestamp range")?;
            let mut entry = CredentialEntry::oauth(
                secret_string(access_token)?,
                refresh_token.as_ref().map(secret_string).transpose()?,
                expires_at,
                token_type,
                scopes.clone(),
            );
            entry.set_upstream_identity(upstream_identity.clone());
            entry
        },
    })
}

fn secret_string(secret: &SecretBytes) -> anyhow::Result<SecretString> {
    Ok(SecretString::from(
        std::str::from_utf8(secret.expose())
            .context("credential token is not UTF-8")?
            .to_owned(),
    ))
}
