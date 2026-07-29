//! Exact provider selection for mount creation.
//!
//! A selector is either a local artifact path, an embedded provider name, or
//! a lowercase digest prefix. Resolution always ends at one validated
//! `ProviderRef` and its manifest. Provider names never select retained
//! artifacts by recency.

use anyhow::{Context as _, anyhow, bail};
use omnifs_api::ProviderMetadata;
use omnifs_core::{ProviderId, ProviderMeta, ProviderName, ProviderRef, ProviderVersion};
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

    pub(crate) async fn resolve(&self, selector: &str) -> anyhow::Result<ResolvedProvider> {
        let path = Path::new(selector);
        match fs::symlink_metadata(path) {
            Ok(metadata) => return self.resolve_path(path, &metadata).await,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {},
            Err(error) => return Err(error).with_context(|| format!("stat provider `{selector}`")),
        }

        if let Some(metadata) = self
            .rpc
            .list_embedded_providers()
            .await?
            .into_iter()
            .find(|metadata| metadata.reference.name == selector)
        {
            return self.resolve_embedded(metadata).await;
        }
        if is_digest_prefix(selector) {
            return self.resolve_digest(selector).await;
        }
        bail!(
            "provider selector `{selector}` is not an existing WASM path, embedded provider name, or lowercase digest prefix"
        )
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

    async fn resolve_artifact(
        &self,
        artifact: &omnifs_provider::Artifact,
    ) -> anyhow::Result<ResolvedProvider> {
        let id = artifact.id();
        if self.rpc.provider_metadata(id).await?.is_some() {
            return self.resolve_id(id).await;
        }
        let receipt = self.import_artifact(artifact).await?;
        anyhow::ensure!(
            receipt.provider.id == id,
            "daemon imported provider `{}` for requested artifact `{id}`",
            receipt.provider.id
        );
        self.resolve_id(id).await
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
        let metadata = self
            .rpc
            .provider_metadata(id)
            .await?
            .ok_or_else(|| anyhow!("provider artifact `{id}` is not retained by the daemon"))?;
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
        let reference = provider_reference(&metadata)?;
        Ok(ResolvedProvider {
            reference,
            manifest,
        })
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

/// One provider choice prepared for `mount add`. Terminal code receives the
/// value, label, and hint separately so it does not know manifest policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderOption {
    pub(crate) name: String,
    pub(crate) hint: String,
    pub(crate) default_selected: bool,
}

pub(crate) fn provider_options(
    embedded: &[ProviderMetadata],
    configured: &BTreeMap<String, String>,
) -> Vec<ProviderOption> {
    let mut options = embedded
        .iter()
        .filter_map(|entry| {
            let manifest = ProviderManifest::from_bytes(&entry.manifest).ok()?;
            (!configured.contains_key(&manifest.id)).then(|| ProviderOption {
                name: manifest.id.clone(),
                hint: manifest
                    .description
                    .clone()
                    .unwrap_or_else(|| manifest.display_name.clone()),
                default_selected: default_selected(&manifest),
            })
        })
        .collect::<Vec<_>>();
    options.sort_by(|left, right| {
        right
            .default_selected
            .cmp(&left.default_selected)
            .then_with(|| left.name.cmp(&right.name))
    });
    options
}

/// A provider is initially selected when mount creation can proceed without an
/// interactive config prompt or an unavailable ambient credential. OAuth is
/// intentionally considered selectable here because an interactive mount can complete its
/// browser flow interactively; `--yes` keeps its stricter ambient-only policy.
fn default_selected(manifest: &ProviderManifest) -> bool {
    if manifest.requires_mount_input() {
        return false;
    }
    if manifest.auth.is_none() {
        return true;
    }
    if matches!(
        manifest
            .auth
            .as_ref()
            .and_then(|auth| auth.default_scheme()),
        Some((_, omnifs_auth::AuthScheme::Oauth(_)))
    ) {
        return true;
    }
    let auth_manifest = manifest
        .auth
        .as_ref()
        .map(omnifs_provider::ProviderAuthManifest::wasm_auth_manifest);
    !crate::commands::mount::detect::detect(auth_manifest.as_ref()).is_empty()
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
