//! Provider artifact management commands.

use anyhow::{Context, bail};
use clap::{Args, Subcommand};
use omnifs_provider::{Artifact, ArtifactLoadError, IndexEntry, ProviderStore, StoreError};
use std::path::{Path, PathBuf};

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
}

#[derive(Args, Debug, Clone)]
pub struct AddArgs {
    /// Provider WASM files or directories containing provider WASM files.
    #[arg(required = true)]
    pub paths: Vec<PathBuf>,
}

impl ProvidersArgs {
    pub fn run(self) -> anyhow::Result<()> {
        match self.command {
            ProvidersCommand::Add(args) => args.run(),
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
        .with_context(|| format!("scan provider directory {}", path.display()))?
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("scan provider directory {}", path.display()))?;
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
    anstream::println!(
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
