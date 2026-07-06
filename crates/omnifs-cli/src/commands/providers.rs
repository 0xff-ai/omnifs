//! Provider artifact management commands.

use anyhow::{Context, bail};
use clap::{Args, Subcommand};
use omnifs_api::{ProviderArtifact, ProviderSummary};
use omnifs_workspace::ids::ProviderName;
use omnifs_workspace::provider::{
    Artifact, ArtifactLoadError, IndexEntry, ProviderStore, StoreError,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::error::ExitCode;
use crate::workspace::Workspace;

#[derive(Args, Debug, Clone)]
pub struct ProvidersArgs {
    #[command(subcommand)]
    pub command: ProvidersCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub enum ProvidersCommand {
    /// Install provider WASM artifacts into the local provider store.
    Add(AddArgs),
    /// List installed provider WASM artifacts.
    Ls(LsArgs),
}

#[derive(Args, Debug, Clone)]
pub struct AddArgs {
    /// Provider WASM files or directories containing provider WASM files.
    #[arg(required = true)]
    pub paths: Vec<PathBuf>,
}

#[derive(Args, Debug, Clone, Default)]
pub struct LsArgs {
    /// Emit machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

impl ProvidersArgs {
    pub async fn run(self) -> anyhow::Result<ExitCode> {
        match self.command {
            ProvidersCommand::Add(args) => args.run().map(|()| ExitCode::Success),
            ProvidersCommand::Ls(args) => args.run().await,
        }
    }
}

impl AddArgs {
    fn run(self) -> anyhow::Result<()> {
        let workspace = Workspace::resolve()?;
        let store = ProviderStore::new(&workspace.layout().providers_dir);
        let mut report = AddReport::default();
        for path in &self.paths {
            add_path(&store, path, &mut report)?;
        }
        match (report.installed, report.found) {
            (0, 0) => bail!("no provider WASM artifacts found"),
            (0, _) => bail!("no valid provider WASM artifacts installed"),
            _ => Ok(()),
        }
    }
}

#[derive(serde::Serialize)]
struct ProvidersJson {
    local: Vec<ProviderSummary>,
    daemon: Option<Vec<ProviderSummary>>,
}

impl LsArgs {
    async fn run(self) -> anyhow::Result<ExitCode> {
        let workspace = Workspace::resolve()?;
        let local = provider_summaries(workspace.catalog())?;
        let daemon = workspace.daemon().providers_if_ready().await?;
        if self.json {
            let payload = ProvidersJson { local, daemon };
            anstream::println!("{}", serde_json::to_string(&payload)?);
        } else {
            anstream::print!("{}", render_providers(&local, daemon.as_deref()));
        }
        Ok(ExitCode::Success)
    }
}

fn render_providers(local: &[ProviderSummary], daemon: Option<&[ProviderSummary]>) -> String {
    let mut out = String::new();
    if local.is_empty() {
        let _ = writeln!(out, "No providers installed.");
    } else {
        let _ = writeln!(out, "Providers ({})", local.len());
        for provider in local {
            let latest = provider
                .latest
                .as_ref()
                .map_or_else(|| "none".to_string(), provider_artifact_label);
            let retained = provider
                .installed
                .iter()
                .map(provider_artifact_label)
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(
                out,
                "  {:<14} latest={} retained={}",
                provider.name,
                latest,
                if retained.is_empty() {
                    "none"
                } else {
                    &retained
                }
            );
        }
    }

    if let Some(daemon) = daemon {
        let _ = writeln!(out);
        let _ = writeln!(out, "Daemon providers ({})", daemon.len());
        for provider in daemon {
            let latest = provider
                .latest
                .as_ref()
                .map_or_else(|| "none".to_string(), provider_artifact_label);
            let _ = writeln!(out, "  {:<14} latest={}", provider.name, latest);
        }
    }
    out
}

fn provider_summaries(
    catalog: &omnifs_workspace::provider::Catalog,
) -> anyhow::Result<Vec<ProviderSummary>> {
    let mut by_name = BTreeMap::new();
    for provider in catalog.installed()? {
        by_name
            .entry(provider.meta.name.clone())
            .or_insert_with(Vec::new)
            .push(provider_artifact(&provider));
    }
    for artifacts in by_name.values_mut() {
        artifacts.sort_by(|a, b| {
            a.version
                .cmp(&b.version)
                .then_with(|| a.id_hash.cmp(&b.id_hash))
        });
    }

    let mut names = catalog
        .installable()?
        .into_iter()
        .map(|provider| provider.meta.name)
        .collect::<BTreeSet<_>>();
    names.extend(by_name.keys().cloned());

    names
        .into_iter()
        .map(|name| provider_summary(catalog, &name, &mut by_name))
        .collect()
}

fn provider_summary(
    catalog: &omnifs_workspace::provider::Catalog,
    name: &ProviderName,
    by_name: &mut BTreeMap<ProviderName, Vec<ProviderArtifact>>,
) -> anyhow::Result<ProviderSummary> {
    let latest = catalog
        .latest_by_name(name)?
        .map(|provider| provider_artifact(&provider));
    Ok(ProviderSummary {
        installed: by_name.remove(name).unwrap_or_default(),
        name: name.to_string(),
        latest,
    })
}

fn provider_artifact(provider: &omnifs_workspace::provider::Provider) -> ProviderArtifact {
    ProviderArtifact {
        version: provider.meta.version.as_ref().map(ToString::to_string),
        id_hash: provider.id.to_string(),
    }
}

fn provider_artifact_label(artifact: &ProviderArtifact) -> String {
    match &artifact.version {
        Some(version) => format!("{version}@{}", artifact.id_hash),
        None => artifact.id_hash.clone(),
    }
}

#[derive(Default)]
struct AddReport {
    found: usize,
    installed: usize,
}

fn add_path(store: &ProviderStore, path: &Path, report: &mut AddReport) -> anyhow::Result<()> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("stat provider path {}", path.display()))?;
    if metadata.is_dir() {
        add_dir(store, path, report)
    } else if metadata.is_file() {
        report.found += 1;
        match add_file(store, path) {
            Ok(entry) => {
                report.installed += 1;
                print_installed(&entry);
                Ok(())
            },
            Err(AddError::Load(ArtifactLoadError::Artifact(error))) => {
                bail!(
                    "provider artifact {} is invalid: {error}",
                    display_path(path)
                )
            },
            Err(AddError::Load(error)) => Err(error.into()),
            Err(AddError::Store(error)) => Err(error.into()),
        }
    } else {
        bail!(
            "provider path {} is not a file or directory",
            path.display()
        )
    }
}

fn add_dir(store: &ProviderStore, path: &Path, report: &mut AddReport) -> anyhow::Result<()> {
    let mut entries = std::fs::read_dir(path)
        .with_context(|| format!("open provider directory {}", path.display()))?
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("read provider directory entry in {}", path.display()))?;
    entries.sort_by_key(std::fs::DirEntry::path);

    for entry in entries {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "wasm") {
            continue;
        }
        report.found += 1;
        match add_file(store, &path) {
            Ok(entry) => {
                report.installed += 1;
                print_installed(&entry);
            },
            Err(AddError::Load(ArtifactLoadError::Artifact(error))) => {
                anstream::eprintln!(
                    "Skipped invalid provider WASM {}: {error}",
                    display_path(&path)
                );
            },
            Err(AddError::Load(error)) => return Err(error.into()),
            Err(AddError::Store(error)) => return Err(error.into()),
        }
    }
    Ok(())
}

fn add_file(store: &ProviderStore, path: &Path) -> Result<IndexEntry, AddError> {
    let artifact = Artifact::from_file(path)?;
    Ok(store.add_artifact(artifact)?)
}

fn print_installed(entry: &IndexEntry) {
    anstream::eprintln!(
        "Installed provider `{}` {} from {}",
        entry.name,
        crate::style::dim(entry.id.to_string()),
        entry.file
    );
}

fn display_path(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map_or_else(|| path.display().to_string(), ToOwned::to_owned)
}

enum AddError {
    Load(ArtifactLoadError),
    Store(StoreError),
}

impl From<ArtifactLoadError> for AddError {
    fn from(error: ArtifactLoadError) -> Self {
        Self::Load(error)
    }
}

impl From<StoreError> for AddError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}
