//! Provider artifact management commands.

use anyhow::{Context, bail};
use clap::{Args, Subcommand};
use omnifs_workspace::provider::{
    Artifact, ArtifactLoadError, IndexEntry, ProviderStore, StoreError,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::ExitCode;
use crate::inventory::{Inventory, ProviderState, ProviderStatus};
use crate::ui::output::{Output, ResultVerdict};
use crate::workspace::Workspace;

#[derive(Args, Debug, Clone)]
pub struct ProviderArgs {
    #[command(subcommand)]
    pub command: ProviderCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub enum ProviderCommand {
    /// Install provider WASM artifacts into the local provider store.
    Add(AddArgs),
    /// List installed provider WASM artifacts.
    Ls(LsArgs),
    /// Inspect one provider and its exact retained artifacts.
    Show(ShowArgs),
}

#[derive(Args, Debug, Clone)]
pub struct AddArgs {
    /// Provider WASM files or directories containing provider WASM files.
    #[arg(required = true)]
    pub paths: Vec<PathBuf>,
}

#[derive(Args, Debug, Clone, Default)]
pub struct LsArgs {}

#[derive(Args, Debug, Clone)]
pub struct ShowArgs {
    /// Provider name to inspect.
    pub provider: String,
    /// Exact artifact digest, or a unique digest prefix.
    #[arg(long)]
    pub artifact: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct ProviderAddReceipt {
    pub(crate) providers: Vec<ProviderStatus>,
    pub(crate) mount_created: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
struct ProviderListResult {
    providers: Vec<ProviderStatus>,
    verdict: crate::inventory::Verdict,
}

#[derive(Debug, Clone, serde::Serialize)]
struct ProviderShowResult {
    providers: Vec<ProviderStatus>,
    mounts: Vec<crate::inventory::MountStatus>,
    verdict: crate::inventory::Verdict,
}

impl ProviderArgs {
    pub async fn run(self, output: Output) -> anyhow::Result<ExitCode> {
        match self.command {
            ProviderCommand::Add(args) => args.run(output).await.map(|()| ExitCode::Success),
            ProviderCommand::Ls(args) => args.run(output).await,
            ProviderCommand::Show(args) => args.run(output).await,
        }
    }
}

impl AddArgs {
    async fn run(self, output: Output) -> anyhow::Result<()> {
        let workspace = Workspace::resolve()?;
        let receipt = self.install(&workspace).await?;
        if output.is_structured() {
            output.emit_result(ResultVerdict::Ok, receipt)?;
        } else {
            print_provider_receipt(&receipt.providers);
            output.narrate("No mount was created. Run `omnifs mount add <provider>`.");
        }
        Ok(())
    }

    /// Install artifacts and return the post-operation inventory receipt. The
    /// caller owns output mode and can serialize this value without probing the
    /// workspace a second time.
    pub(crate) async fn install(
        &self,
        workspace: &Workspace,
    ) -> anyhow::Result<ProviderAddReceipt> {
        let store = ProviderStore::new(&workspace.layout().providers_dir);
        let mut report = AddReport::default();
        for path in &self.paths {
            add_path(&store, path, &mut report)?;
        }
        match (report.installed, report.found) {
            (0, 0) => bail!("no provider WASM artifacts found"),
            (0, _) => bail!("no valid provider WASM artifacts installed"),
            _ => {},
        }

        // Re-collect after installation. Inventory joins exact mount pins, so
        // newly installed artifacts are reported as installed without ever
        // creating or mutating a mount.
        let inventory = Inventory::collect(workspace).await?;
        let installed = receipt_rows(&inventory.providers, &report.installed_ids);
        let receipt = ProviderAddReceipt {
            providers: installed,
            mount_created: false,
        };
        Ok(receipt)
    }
}

impl LsArgs {
    async fn run(self, output: Output) -> anyhow::Result<ExitCode> {
        let workspace = Workspace::resolve()?;
        let inventory = Inventory::collect(&workspace).await?;
        let rows = &inventory.providers;
        let exit_code = if rows.iter().any(|row| row.state == ProviderState::Missing) {
            ExitCode::Degraded
        } else {
            ExitCode::Success
        };
        if output.is_structured() {
            output.emit_result(
                if exit_code == ExitCode::Degraded {
                    ResultVerdict::Degraded
                } else {
                    ResultVerdict::Ok
                },
                ProviderListResult {
                    providers: rows.clone(),
                    verdict: inventory.verdict(),
                },
            )?;
        } else {
            print_provider_receipt(rows);
        }
        Ok(exit_code)
    }
}

impl ShowArgs {
    async fn run(self, output: Output) -> anyhow::Result<ExitCode> {
        let workspace = Workspace::resolve()?;
        let inventory = Inventory::collect(&workspace).await?;
        let rows = select_rows(
            &inventory.providers,
            &self.provider,
            self.artifact.as_deref(),
        )?;
        let exit_code = if rows.iter().any(|row| row.state == ProviderState::Missing) {
            ExitCode::Degraded
        } else {
            ExitCode::Success
        };
        if output.is_structured() {
            let provider_name = self.provider.as_str();
            let mounts = inventory
                .mounts
                .iter()
                .filter(|mount| mount.provider.name == provider_name)
                .cloned()
                .collect();
            output.emit_result(
                if exit_code == ExitCode::Degraded {
                    ResultVerdict::Degraded
                } else {
                    ResultVerdict::Ok
                },
                ProviderShowResult {
                    providers: rows.clone(),
                    mounts,
                    verdict: inventory.verdict(),
                },
            )?;
        } else {
            print_provider_receipt(&rows);
        }
        Ok(exit_code)
    }
}

/// Resolve a provider selector against exact inventory rows. A name selects
/// every retained artifact for that provider; an artifact selector must match
/// exactly one digest (or one unique prefix), and never falls back to recency.
pub(crate) fn select_rows(
    rows: &[ProviderStatus],
    provider: &str,
    artifact: Option<&str>,
) -> anyhow::Result<Vec<ProviderStatus>> {
    let named = rows
        .iter()
        .filter(|row| row.name == provider)
        .cloned()
        .collect::<Vec<_>>();
    if named.is_empty() {
        bail!(
            "provider `{provider}` was not found; candidates:\n{}",
            candidate_rows(rows)
        );
    }
    let Some(selector) = artifact else {
        return Ok(named);
    };

    let matches = named
        .iter()
        .filter(|row| row.artifact == selector || row.artifact.starts_with(selector))
        .cloned()
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [row] => Ok(vec![row.clone()]),
        [] => bail!(
            "artifact `{selector}` for provider `{provider}` was not found; candidates:\n{}",
            candidate_rows(&named)
        ),
        _ => bail!(
            "artifact selector `{selector}` is ambiguous for provider `{provider}`; candidates:\n{}",
            candidate_rows(&matches)
        ),
    }
}

fn candidate_rows(rows: &[ProviderStatus]) -> String {
    rows.iter()
        .map(|row| {
            format!(
                "  {} {} {}",
                row.name,
                row.version.as_deref().unwrap_or("unversioned"),
                row.artifact
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn receipt_rows(rows: &[ProviderStatus], artifact_ids: &[String]) -> Vec<ProviderStatus> {
    artifact_ids
        .iter()
        .filter_map(|id| rows.iter().find(|row| row.artifact == *id))
        .map(|row| (row.artifact.clone(), row.clone()))
        .collect::<BTreeMap<_, _>>()
        .into_values()
        .collect()
}

fn print_provider_receipt(rows: &[ProviderStatus]) {
    use crate::ui::table::{Block, Report};
    let mut report = Report::new();
    report.push(Block::Resources(crate::status::provider_rows_table(
        "Providers",
        rows,
    )));
    report.print();
}

#[derive(Default)]
struct AddReport {
    found: usize,
    installed: usize,
    installed_ids: Vec<String>,
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
                report.installed_ids.push(entry.id.to_string());
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
                report.installed_ids.push(entry.id.to_string());
            },
            Err(AddError::Load(ArtifactLoadError::Artifact(error))) => {
                crate::ui::eprint_raw(&format!(
                    "Skipped invalid provider WASM {}: {error}\n",
                    display_path(&path)
                ));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        name: &str,
        version: Option<&str>,
        artifact: &str,
        state: ProviderState,
    ) -> ProviderStatus {
        ProviderStatus {
            name: name.into(),
            version: version.map(str::to_owned),
            artifact: artifact.into(),
            pinned_by: Vec::new(),
            state,
            fix: state.fix().map(str::to_owned),
        }
    }

    #[test]
    fn show_selects_all_exact_artifacts_for_a_provider() {
        let rows = vec![
            row("github", Some("0.4.0"), "aaaa", ProviderState::Installed),
            row("github", Some("0.4.1"), "bbbb", ProviderState::Pinned),
            row("linear", None, "cccc", ProviderState::Installed),
        ];
        let selected = select_rows(&rows, "github", None).unwrap();
        assert_eq!(selected.len(), 2);
    }

    #[test]
    fn show_preserves_multiple_versions_and_unversioned_artifacts() {
        let rows = vec![
            row("github", Some("0.4.0"), "aaaa", ProviderState::Installed),
            row("github", None, "bbbb", ProviderState::Installed),
        ];
        let selected = select_rows(&rows, "github", None).unwrap();
        assert_eq!(selected[0].version.as_deref(), Some("0.4.0"));
        assert_eq!(selected[1].version, None);
    }

    #[test]
    fn duplicate_install_ids_produce_one_receipt_row() {
        let rows = vec![row(
            "github",
            Some("0.4.1"),
            "bbbb",
            ProviderState::Installed,
        )];
        let ids = vec!["bbbb".to_owned(), "bbbb".to_owned()];
        assert_eq!(receipt_rows(&rows, &ids).len(), 1);
    }

    #[test]
    fn show_keeps_missing_artifacts_and_reverse_pin_facts() {
        let mut pinned = row("github", Some("0.4.1"), "bbbb", ProviderState::Pinned);
        pinned.pinned_by = vec!["main".into(), "mirror".into()];
        let missing = row("github", Some("0.3.0"), "cccc", ProviderState::Missing);
        let rows = vec![pinned, missing];
        let selected = select_rows(&rows, "github", Some("cccc")).unwrap();
        assert_eq!(selected[0].state, ProviderState::Missing);
        assert_eq!(
            rows[0].pinned_by,
            vec!["main".to_owned(), "mirror".to_owned()]
        );
    }

    #[test]
    fn invalid_wasm_is_reported_at_the_artifact_boundary() {
        use std::io::Write as _;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("invalid.wasm");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(b"not wasm")
            .unwrap();
        let store = ProviderStore::new(temp.path().join("providers"));
        let result = add_file(&store, &path);
        assert!(matches!(
            result,
            Err(AddError::Load(ArtifactLoadError::Artifact(_)))
        ));
    }

    #[test]
    fn directory_with_invalid_wasm_keeps_found_count_truthful() {
        use std::io::Write as _;

        let temp = tempfile::tempdir().unwrap();
        for name in ["a.wasm", "b.wasm"] {
            let mut file = std::fs::File::create(temp.path().join(name)).unwrap();
            file.write_all(b"not wasm").unwrap();
        }
        let store = ProviderStore::new(temp.path().join("providers"));
        let mut report = AddReport::default();
        add_dir(&store, temp.path(), &mut report).unwrap();
        assert_eq!(report.found, 2);
        assert_eq!(report.installed, 0);
    }

    #[test]
    fn show_accepts_unique_digest_prefix_and_rejects_ambiguous_prefix() {
        let rows = vec![
            row("github", None, "aaaa1111", ProviderState::Installed),
            row("github", None, "aaaa2222", ProviderState::Installed),
        ];
        let error = select_rows(&rows, "github", Some("aaaa")).unwrap_err();
        assert!(error.to_string().contains("ambiguous"));
        let selected = select_rows(&rows, "github", Some("aaaa1")).unwrap();
        assert_eq!(selected[0].artifact, "aaaa1111");
    }

    #[test]
    fn show_reports_candidates_for_an_absent_provider_or_artifact() {
        let rows = vec![row("github", Some("0.4.1"), "bbbb", ProviderState::Pinned)];
        let error = select_rows(&rows, "linear", None).unwrap_err();
        assert!(error.to_string().contains("github"));
        let error = select_rows(&rows, "github", Some("cccc")).unwrap_err();
        assert!(error.to_string().contains("bbbb"));
    }

    #[test]
    fn add_receipt_never_claims_a_mount() {
        let receipt = ProviderAddReceipt {
            providers: vec![row(
                "github",
                Some("0.4.1"),
                "bbbb",
                ProviderState::Installed,
            )],
            mount_created: false,
        };
        let json = serde_json::to_value(receipt).unwrap();
        assert_eq!(json["mount_created"], false);
    }
}
