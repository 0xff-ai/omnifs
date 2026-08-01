//! Exact provider selection for mount creation.
//!
//! A selector is either a local artifact path, an embedded provider name, or
//! a lowercase digest prefix. Resolution always ends at one validated
//! `ProviderRef` and its manifest. Provider names never select retained
//! artifacts by recency.

use anyhow::{Context as _, anyhow, bail};
use omnifs_api::ProviderMetadata;
use omnifs_core::{ProviderId, ProviderMeta, ProviderName, ProviderRef, ProviderVersion};
use omnifs_kcl::{AuthoringResource, EvaluatedConfig, ProviderSource, resolve_local_source};
use omnifs_provider::ProviderManifest;
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

use crate::rpc::RpcClient;

pub(crate) struct ResolvedProvider {
    pub(crate) reference: ProviderRef,
    pub(crate) manifest: ProviderManifest,
}

pub(crate) struct ProviderResolver<'a> {
    rpc: &'a RpcClient,
}

impl<'a> ProviderResolver<'a> {
    pub(crate) fn new(rpc: &'a RpcClient) -> Self {
        Self { rpc }
    }

    /// `embedded` is the caller's already-fetched embedded provider listing
    /// (every caller needs it anyway, to build a picker or validate a name),
    /// so a name selector never re-fetches the same bundle listing here.
    pub(crate) async fn resolve(
        &self,
        selector: &str,
        embedded: &[ProviderMetadata],
    ) -> anyhow::Result<ResolvedProvider> {
        let path = Path::new(selector);
        match fs::symlink_metadata(path) {
            Ok(metadata) => return self.resolve_path(path, &metadata).await,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {},
            Err(error) => return Err(error).with_context(|| format!("stat provider `{selector}`")),
        }

        if let Some(metadata) = embedded
            .iter()
            .find(|metadata| metadata.reference.name == selector)
        {
            return self.resolve_embedded(metadata.clone()).await;
        }
        if is_digest_prefix(selector) {
            return self.resolve_digest(selector).await;
        }
        bail!(
            "provider selector `{selector}` is not an existing WASM path, embedded provider name, or lowercase digest prefix"
        )
    }

    /// Resolve only a local Wasm path.
    ///
    /// Interactive provider authoring calls this after the operator chose the
    /// local-file branch so a missing path can never fall through to an
    /// embedded name or retained digest selector.
    pub(crate) async fn resolve_local(&self, path: &Path) -> anyhow::Result<ResolvedProvider> {
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("stat local provider {}", path.display()))?;
        self.resolve_path(path, &metadata).await
    }

    async fn resolve_path(
        &self,
        path: &Path,
        metadata: &fs::Metadata,
    ) -> anyhow::Result<ResolvedProvider> {
        if metadata.is_dir() {
            let wasm_files = fs::read_dir(path)
                .with_context(|| format!("read provider directory {}", path.display()))?
                .collect::<Result<Vec<_>, _>>()
                .with_context(|| format!("read provider directory {}", path.display()))?
                .into_iter()
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "wasm"))
                .map(|entry| {
                    let path = entry.path();
                    let metadata = fs::symlink_metadata(&path)
                        .with_context(|| format!("stat provider artifact {}", path.display()))?;
                    if !metadata.file_type().is_file() {
                        bail!("provider artifact {} is not a regular file", path.display());
                    }
                    Ok(path)
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            let [wasm] = wasm_files.as_slice() else {
                bail!(
                    "provider directory {} must contain exactly one regular `.wasm` file",
                    path.display()
                );
            };
            return self.resolve_file(wasm).await;
        }
        if !metadata.file_type().is_file() {
            bail!(
                "provider path {} is not a regular file or directory",
                path.display()
            );
        }
        self.resolve_file(path).await
    }

    async fn resolve_file(&self, path: &Path) -> anyhow::Result<ResolvedProvider> {
        let artifact = omnifs_provider::Artifact::from_file(path)
            .with_context(|| format!("validate provider artifact {}", path.display()))?;
        self.resolve_artifact(&artifact).await
    }

    async fn resolve_digest(&self, selector: &str) -> anyhow::Result<ResolvedProvider> {
        let mut ids = BTreeMap::<String, ProviderId>::new();
        for provider in self.rpc.list_providers().await? {
            let id = provider.reference.id;
            if id.to_string().starts_with(selector) {
                ids.insert(id.to_string(), id);
            }
        }
        let matches = ids.into_values().collect::<Vec<_>>();
        let id = match matches.as_slice() {
            [id] => *id,
            [] => bail!(
                "provider digest prefix `{selector}` did not match a retained daemon provider"
            ),
            _ => bail!("provider digest prefix `{selector}` is ambiguous"),
        };
        self.resolve_id(id).await
    }

    /// Fetches `provider_metadata(id)` at most once on either branch: the
    /// probe's own `Some` is used directly when the artifact is already
    /// retained, and only the not-retained branch pays for a second fetch
    /// (unavoidable, since import only returns a reference, not the
    /// manifest bytes `resolved_from_metadata` needs).
    async fn resolve_artifact(
        &self,
        artifact: &omnifs_provider::Artifact,
    ) -> anyhow::Result<ResolvedProvider> {
        let id = artifact.id();
        let metadata = if let Some(metadata) = self.rpc.provider_metadata(id).await? {
            metadata
        } else {
            let receipt = self.import_artifact(artifact).await?;
            anyhow::ensure!(
                receipt.provider.id == id,
                "daemon imported provider `{}` for requested artifact `{id}`",
                receipt.provider.id
            );
            self.fetch_retained(id).await?
        };
        resolved_from_metadata(id, &metadata)
    }

    async fn resolve_embedded(
        &self,
        metadata: ProviderMetadata,
    ) -> anyhow::Result<ResolvedProvider> {
        let id = metadata.reference.id;
        if self.rpc.provider_metadata(id).await?.is_none() {
            let receipt = self.import_embedded(&metadata.reference.name).await?;
            anyhow::ensure!(
                receipt.provider.id == id,
                "daemon imported a different embedded provider"
            );
        }
        let manifest = ProviderManifest::from_bytes(&metadata.manifest)
            .context("validate embedded provider metadata")?;
        anyhow::ensure!(
            manifest.id == metadata.reference.name,
            "embedded provider metadata name `{}` does not match manifest `{}`",
            metadata.reference.name,
            manifest.id
        );
        let reference = provider_reference(&metadata)?;
        anyhow::ensure!(reference.id == id, "embedded provider metadata id mismatch");
        Ok(ResolvedProvider {
            reference,
            manifest,
        })
    }

    async fn resolve_id(&self, id: ProviderId) -> anyhow::Result<ResolvedProvider> {
        let metadata = self.fetch_retained(id).await?;
        resolved_from_metadata(id, &metadata)
    }

    /// The one place that fetches a retained provider's metadata and turns
    /// its absence into an error; every caller that already knows the
    /// artifact is retained (or just imported it) routes through here
    /// instead of re-deriving the same "not retained" message.
    async fn fetch_retained(&self, id: ProviderId) -> anyhow::Result<ProviderMetadata> {
        self.rpc
            .provider_metadata(id)
            .await?
            .ok_or_else(|| anyhow!("provider artifact `{id}` is not retained by the daemon"))
    }

    async fn import_artifact(
        &self,
        artifact: &omnifs_provider::Artifact,
    ) -> anyhow::Result<omnifs_api::ProviderImportReceipt> {
        self.rpc
            .import_provider(artifact.file().to_owned(), artifact.bytes())
            .await
    }

    async fn import_embedded(
        &self,
        name: &str,
    ) -> anyhow::Result<omnifs_api::ProviderImportReceipt> {
        self.rpc.import_embedded_provider(name.to_owned()).await
    }
}

/// Validate one daemon-retained provider's metadata and build the resolved
/// value from it. The one owner of this validation, so every path that ends
/// at a retained-by-id artifact (a digest match, a fresh import, an
/// already-retained probe) agrees on what "valid" means without each
/// re-fetching metadata just to reach this same check.
fn resolved_from_metadata(
    id: ProviderId,
    metadata: &ProviderMetadata,
) -> anyhow::Result<ResolvedProvider> {
    let manifest = ProviderManifest::from_bytes(&metadata.manifest)
        .with_context(|| format!("validate daemon metadata for provider `{id}`"))?;
    anyhow::ensure!(
        manifest.id == metadata.reference.name,
        "daemon metadata name `{}` does not match manifest `{}`",
        metadata.reference.name,
        manifest.id
    );
    anyhow::ensure!(
        metadata.reference.id == id,
        "daemon metadata returned provider `{}` for requested `{id}`",
        metadata.reference.id
    );
    let reference = provider_reference(metadata)?;
    Ok(ResolvedProvider {
        reference,
        manifest,
    })
}

fn provider_reference(metadata: &ProviderMetadata) -> anyhow::Result<ProviderRef> {
    Ok(ProviderRef {
        id: metadata.reference.id,
        meta: ProviderMeta {
            name: ProviderName::new(metadata.reference.name.clone())
                .context("daemon returned invalid provider name")?,
            version: metadata.reference.version.clone().map(ProviderVersion::new),
        },
    })
}

fn is_digest_prefix(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Resolve KCL provider selectors to daemon-retained content identities.
pub(crate) async fn resolve_kcl_sources(
    evaluated: &EvaluatedConfig,
    rpc: &RpcClient,
) -> anyhow::Result<BTreeMap<omnifs_core::ResourceName, ProviderId>> {
    let config_dir = evaluated
        .source
        .parent()
        .context("KCL source has no parent directory")?;
    let mut resolved = BTreeMap::new();
    for resource in &evaluated.config.resources {
        let AuthoringResource::Provider(provider) = resource else {
            continue;
        };
        let digest = match &provider.source {
            ProviderSource::Embedded { embedded } => {
                rpc.import_embedded_provider(embedded.to_string())
                    .await?
                    .provider
                    .id
            },
            ProviderSource::Digest { digest } => {
                anyhow::ensure!(
                    rpc.provider_metadata(*digest).await?.is_some(),
                    "provider artifact {digest} is not retained"
                );
                *digest
            },
            ProviderSource::Local { local } => {
                let (path, bytes) = resolve_local_source(local, config_dir).await?;
                let digest = local.expected_digest;
                let file_name = path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .context("local provider filename is not valid UTF-8")?
                    .to_owned();
                let receipt = rpc.import_provider(file_name, &bytes).await?;
                anyhow::ensure!(
                    receipt.provider.id == digest,
                    "provider import digest mismatch"
                );
                digest
            },
        };
        resolved.insert(provider.name.clone(), digest);
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_prefix_accepts_only_lowercase_hex() {
        assert!(is_digest_prefix("abc123"));
        assert!(is_digest_prefix(&"a".repeat(64)));
        assert!(!is_digest_prefix(""));
        assert!(!is_digest_prefix("ABC123"));
        assert!(!is_digest_prefix(&"a".repeat(65)));
    }

    #[test]
    fn daemon_reference_becomes_core_reference_without_local_store() {
        let id = ProviderId::from_digest([7; 32]);
        let metadata = ProviderMetadata {
            reference: omnifs_api::ProviderReference {
                id,
                name: "demo".to_owned(),
                version: Some("1.2.3".to_owned()),
            },
            manifest: Vec::new(),
        };
        let reference = provider_reference(&metadata).unwrap();
        assert_eq!(reference.id, id);
        assert_eq!(reference.meta.name.as_str(), "demo");
        assert_eq!(
            reference.meta.version.as_ref().map(ProviderVersion::as_str),
            Some("1.2.3")
        );
    }
}
