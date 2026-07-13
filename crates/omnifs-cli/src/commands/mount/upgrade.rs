//! Explicit provider-artifact selection and mount repinning.

use anyhow::{Context, anyhow, bail};
use clap::Args;
use semver::Version;
use serde::Serialize;

use omnifs_workspace::creds::FileStore;
use omnifs_workspace::ids::ProviderId;
use omnifs_workspace::mounts::{
    AuthDelta, CapabilityChange, CapabilityDirection, FieldChange, LimitChange, LimitDirection,
    Name as MountName, Spec, UpgradePlan,
};
use omnifs_workspace::provider::Provider;

use crate::error::ExitCode;
use crate::inventory::{AccessPath, Inventory};
use crate::mount_config::MountConfig;
use crate::ui::output::{Output, ResultVerdict};
use crate::ui::prompt::Confirm;
use crate::ui::table::StateToken;
use crate::workspace::Workspace;

#[derive(Args, Debug, Clone)]
pub struct UpgradeArgs {
    /// Existing mount to repin.
    pub name: String,
    /// Exact provider digest or provider version label.
    #[arg(long = "to")]
    pub to: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct CandidateSummary {
    pub(crate) id: String,
    pub(crate) provider: String,
    pub(crate) version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub(crate) id: ProviderId,
    pub(crate) provider: String,
    pub(crate) version: Option<String>,
}

impl Candidate {
    fn from_provider(provider: &Provider) -> Self {
        Self {
            id: provider.id,
            provider: provider.meta.name.to_string(),
            version: provider.meta.version.as_ref().map(ToString::to_string),
        }
    }
}

impl CandidateSummary {
    fn from_candidate(candidate: &Candidate) -> Self {
        Self {
            id: candidate.id.to_string(),
            provider: candidate.provider.clone(),
            version: candidate.version.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct UpgradePreview {
    pub(crate) mount: String,
    pub(crate) before: CandidateSummary,
    pub(crate) after: CandidateSummary,
    pub(crate) delta: Vec<String>,
    pub(crate) changed: bool,
    pub(crate) access_paths: Vec<AccessPath>,
}

struct PreparedUpgrade {
    current: Provider,
    candidate: Provider,
    spec: Spec,
    plan: UpgradePlan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Selection {
    Noop,
    Candidate(ProviderId),
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum SelectionError {
    #[error("provider artifact `{0}` is not installed")]
    Missing(String),
    #[error("provider artifact `{target}` belongs to provider `{actual}`, not `{expected}`")]
    WrongProvider {
        target: String,
        expected: String,
        actual: String,
    },
    #[error("version `{version}` is ambiguous; choose one of these digests: {candidates}")]
    AmbiguousVersion { version: String, candidates: String },
    #[error("version `{0}` is not installed")]
    MissingVersion(String),
    #[error(
        "implicit upgrade is unavailable for an unversioned or non-semver current artifact; pass --to VERSION or --to DIGEST"
    )]
    CurrentNotSemver,
    #[error("no installed semver is strictly newer than current version `{0}`")]
    NoNewerVersion(String),
    #[error("highest version `{version}` is ambiguous; choose one of these digests: {candidates}")]
    AmbiguousHighest { version: String, candidates: String },
}

pub(crate) fn describe_upgrade_changes(
    capabilities: &[CapabilityChange],
    limits: &[LimitChange],
    auth: Option<&AuthDelta>,
) -> Vec<String> {
    let mut parts = Vec::new();
    for change in capabilities {
        let direction = match change.direction {
            CapabilityDirection::Added => "added",
            CapabilityDirection::Removed => "removed",
        };
        parts.push(format!(
            "{direction} capability `{}` = `{}`",
            change.kind, change.value
        ));
    }
    for change in limits {
        let direction = match change.direction {
            LimitDirection::Added => "added",
            LimitDirection::Removed => "removed",
        };
        parts.push(format!(
            "{direction} limit `{}` = `{}`",
            change.name, change.value
        ));
    }
    if let Some(auth) = auth {
        parts.push(auth.describe());
    }
    parts
}

pub(crate) fn describe_upgrade_plan(plan: &UpgradePlan) -> Vec<String> {
    match plan {
        UpgradePlan::Identical => Vec::new(),
        UpgradePlan::AdditiveConfig { added } => added
            .iter()
            .map(|field| format!("new optional field `{}`", field.name))
            .collect(),
        UpgradePlan::BreakingConfig { changes } => {
            changes.iter().map(FieldChange::describe).collect()
        },
        UpgradePlan::CapabilityLimitOrAuth {
            capabilities,
            limits,
            auth,
        } => describe_upgrade_changes(capabilities, limits, auth.as_ref()),
    }
}

/// Select a target without consulting install order or the catalog's latest
/// pointer. The caller supplies all retained artifacts for one catalog.
pub(crate) fn select_candidate(
    current: &Candidate,
    candidates: &[Candidate],
    target: Option<&str>,
) -> Result<Selection, SelectionError> {
    if let Some(target) = target {
        if let Ok(id) = target.parse::<ProviderId>() {
            let Some(candidate) = candidates.iter().find(|provider| provider.id == id) else {
                return Err(SelectionError::Missing(target.to_owned()));
            };
            if candidate.provider != current.provider {
                return Err(SelectionError::WrongProvider {
                    target: target.to_owned(),
                    expected: current.provider.clone(),
                    actual: candidate.provider.clone(),
                });
            }
            return Ok(if id == current.id {
                Selection::Noop
            } else {
                Selection::Candidate(id)
            });
        }

        let all_matches = candidates
            .iter()
            .filter(|provider| {
                provider
                    .version
                    .as_deref()
                    .is_some_and(|version| version == target)
            })
            .collect::<Vec<_>>();
        let matches = all_matches
            .iter()
            .copied()
            .filter(|provider| provider.provider == current.provider)
            .collect::<Vec<_>>();
        if matches.is_empty()
            && let Some(provider) = all_matches.first()
        {
            return Err(SelectionError::WrongProvider {
                target: target.to_owned(),
                expected: current.provider.clone(),
                actual: provider.provider.clone(),
            });
        }
        return match matches.as_slice() {
            [] => Err(SelectionError::MissingVersion(target.to_owned())),
            [candidate] => Ok(if candidate.id == current.id {
                Selection::Noop
            } else {
                Selection::Candidate(candidate.id)
            }),
            _ => Err(SelectionError::AmbiguousVersion {
                version: target.to_owned(),
                candidates: format_candidates(&matches),
            }),
        };
    }

    let current_version = current
        .version
        .as_deref()
        .and_then(|version| Version::parse(version).ok())
        .ok_or(SelectionError::CurrentNotSemver)?;
    let mut newer = candidates
        .iter()
        .filter(|provider| provider.provider == current.provider)
        .filter_map(|provider| {
            provider
                .version
                .as_deref()
                .and_then(|version| Version::parse(version).ok())
                .filter(|version| *version > current_version)
                .map(|version| (version, provider))
        })
        .collect::<Vec<_>>();
    if newer.is_empty() {
        return Err(SelectionError::NoNewerVersion(current_version.to_string()));
    }
    newer.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| left.1.id.to_string().cmp(&right.1.id.to_string()))
    });
    let highest = newer.last().expect("newer is non-empty").0.clone();
    let highest_matches = newer
        .into_iter()
        .filter(|(version, _)| *version == highest)
        .map(|(_, provider)| provider)
        .collect::<Vec<_>>();
    if highest_matches.len() != 1 {
        return Err(SelectionError::AmbiguousHighest {
            version: highest.to_string(),
            candidates: format_candidates(&highest_matches),
        });
    }
    Ok(Selection::Candidate(highest_matches[0].id))
}

fn format_candidates(candidates: &[&Candidate]) -> String {
    candidates
        .iter()
        .map(|provider| {
            format!(
                "{}{}",
                provider.id,
                provider
                    .version
                    .as_ref()
                    .map_or(String::new(), |version| format!(" ({version})"))
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

impl UpgradeArgs {
    pub async fn run(self, output: Output) -> anyhow::Result<ExitCode> {
        let workspace = Workspace::resolve()?;
        let preview = self.run_with_output(&workspace, output).await?;
        if output.mode().is_structured() {
            output.emit_result(ResultVerdict::Ok, &preview)?;
        } else {
            crate::ui::print_raw(&render_upgrade_receipt(&preview));
        }
        Ok(ExitCode::Success)
    }

    pub(crate) async fn run_with_output(
        &self,
        workspace: &Workspace,
        output: Output,
    ) -> anyhow::Result<UpgradePreview> {
        let prepared = self.prepare(workspace)?;
        let inventory = Inventory::collect(workspace).await?;
        let mount_name = MountName::new(self.name.clone())?;
        let current = Candidate::from_provider(&prepared.current);
        let candidate = Candidate::from_provider(&prepared.candidate);
        let preview = UpgradePreview {
            mount: self.name.clone(),
            before: CandidateSummary::from_candidate(&current),
            after: CandidateSummary::from_candidate(&candidate),
            delta: describe_upgrade_plan(&prepared.plan),
            changed: prepared.candidate.id != prepared.current.id,
            access_paths: inventory.access_paths(&mount_name),
        };
        if !preview.changed {
            return Ok(preview);
        }
        if !output.is_structured() {
            crate::ui::print_raw(&render_upgrade_review(&preview));
        }
        self.apply(workspace, output, prepared, preview).await
    }

    fn prepare(&self, workspace: &Workspace) -> anyhow::Result<PreparedUpgrade> {
        let mounts = workspace.mounts()?;
        let mount = mounts
            .iter()
            .find(|mount| mount.name.as_str() == self.name)
            .ok_or_else(|| anyhow!("no mount named `{}`", self.name))?;
        let current = workspace
            .catalog()
            .get(&mount.config.provider.id)?
            .ok_or_else(|| {
                anyhow!(
                    "provider artifact `{}` for mount `{}` is not installed",
                    mount.config.provider.id,
                    self.name
                )
            })?;
        let candidates = workspace.catalog().installed()?;
        let current_label = Candidate::from_provider(&current);
        let candidate_labels = candidates
            .iter()
            .map(Candidate::from_provider)
            .collect::<Vec<_>>();
        let selection = select_candidate(&current_label, &candidate_labels, self.to.as_deref())
            .map_err(|error| anyhow!("cannot upgrade mount `{}`: {error}", self.name))?;
        let candidate = match selection {
            Selection::Noop => current.clone(),
            Selection::Candidate(id) => candidates
                .iter()
                .find(|provider| provider.id == id)
                .cloned()
                .ok_or_else(|| anyhow!("selected provider artifact `{id}` disappeared"))?,
        };
        let before_manifest = current.manifest()?;
        let after_manifest = candidate.manifest()?;
        let plan = UpgradePlan::diff(&before_manifest, &after_manifest);
        let mut spec = mount.config.clone();
        if candidate.id != current.id {
            match &plan {
                UpgradePlan::Identical | UpgradePlan::CapabilityLimitOrAuth { .. } => {},
                UpgradePlan::AdditiveConfig { added } => {
                    spec.apply_provider_metadata(
                        &after_manifest,
                        omnifs_workspace::mounts::ProviderMetadataInheritance::additive_config(
                            added,
                        ),
                    )?;
                },
                UpgradePlan::BreakingConfig { changes } => {
                    let details = changes
                        .iter()
                        .map(omnifs_workspace::mounts::FieldChange::describe)
                        .collect::<Vec<_>>()
                        .join(", ");
                    bail!(
                        "cannot repin mount `{}` safely: provider config changed ({details}); reinitialize the mount",
                        self.name
                    );
                },
            }
            spec.provider = candidate.reference();
            validate_spec(&spec, mount, workspace, &after_manifest)?;
        }
        Ok(PreparedUpgrade {
            current,
            candidate,
            spec,
            plan,
        })
    }

    async fn apply(
        &self,
        workspace: &Workspace,
        output: Output,
        prepared: PreparedUpgrade,
        preview: UpgradePreview,
    ) -> anyhow::Result<UpgradePreview> {
        if !output.yes() {
            if output.no_input()
                || output.mode().is_structured()
                || !crate::ui::prompt::is_terminal()
            {
                bail!("mount upgrade requires confirmation; pass --yes in non-interactive mode");
            }
            if !Confirm::new(format!("Repin mount `{}`?", self.name)).ask_with_output(output)? {
                bail!("mount upgrade canceled");
            }
        }
        workspace
            .put_mount(&prepared.spec)
            .context("persist upgraded mount spec")?;
        if let Some(report) = workspace
            .daemon()
            .update_mount_if_ready(&prepared.spec, Some(&prepared.plan))
            .await?
            && let Some(failure) = report.failure
        {
            return Err(anyhow!(
                "mount `{}` repin failed in running daemon: {}",
                self.name,
                failure.reason
            ));
        }
        let inventory = Inventory::collect(workspace).await?;
        let mount_name = MountName::new(self.name.clone())?;
        Ok(UpgradePreview {
            access_paths: inventory.access_paths(&mount_name),
            ..preview
        })
    }
}

fn validate_spec(
    spec: &Spec,
    existing: &MountConfig,
    workspace: &Workspace,
    manifest: &omnifs_workspace::provider::ProviderManifest,
) -> anyhow::Result<()> {
    if let Some(config) = spec.config_raw.as_ref() {
        manifest
            .config
            .as_ref()
            .ok_or_else(|| {
                anyhow!(
                    "provider `{}` has no config metadata for existing config",
                    manifest.id
                )
            })?
            .validate_config(config)
            .map_err(|error| anyhow!("provider config failed validation: {error}"))?;
    } else if manifest.config.as_ref().is_some_and(|config| {
        config
            .fields
            .iter()
            .any(|field| field.required && field.default.is_none())
    }) {
        bail!(
            "provider `{}` requires config fields that the existing mount does not define",
            manifest.id
        );
    }
    omnifs_workspace::mounts::materialize::materialize(spec.clone(), workspace.catalog())
        .context("validate complete upgraded mount spec")?;
    if spec.auth.is_some() {
        let configured = MountConfig {
            name: existing.name.clone(),
            config: spec.clone(),
            source: existing.source.clone(),
        };
        let auth = crate::auth::MountAuth::from_spec(workspace.catalog(), spec.clone());
        let store = FileStore::new(&workspace.layout().credentials_file);
        configured.validate_host_managed_credentials(&auth, &store)?;
    }
    Ok(())
}

fn render_upgrade_review(preview: &UpgradePreview) -> String {
    render_upgrade(preview, StateToken::attention("review required"))
}

fn render_upgrade_receipt(preview: &UpgradePreview) -> String {
    let state = if preview.changed {
        StateToken::positive("updated")
    } else {
        StateToken::neutral("unchanged")
    };
    render_upgrade(preview, state)
}

fn render_upgrade(preview: &UpgradePreview, state: StateToken) -> String {
    use crate::ui::table::{
        Block, Cell, Column, CountLabel, Priority, Report, ResourceRow, ResourceTable, WidthPolicy,
    };

    let mut changes = ResourceTable::new(
        "Provider change",
        CountLabel::number(1),
        vec![
            Column::new("Mount", Priority::Identity, WidthPolicy::Auto),
            Column::new("From", Priority::Essential, WidthPolicy::Digest),
            Column::new("To", Priority::Essential, WidthPolicy::Digest),
            Column::new("Authority", Priority::Detail, WidthPolicy::Auto),
            Column::new("State", Priority::Secondary, WidthPolicy::Auto),
        ],
    );
    let authority = if preview.delta.is_empty() {
        "unchanged".to_owned()
    } else {
        preview.delta.join("; ")
    };
    let from = format_candidate(&preview.before);
    let to = format_candidate(&preview.after);
    changes.push(ResourceRow::new(
        [
            Cell::new(&preview.mount),
            Cell::new(from),
            Cell::new(to),
            Cell::new(authority),
            Cell::state(state.clone()),
        ],
        state,
    ));

    let mut report = Report::new();
    report.push(Block::Resources(changes));
    if !preview.access_paths.is_empty() {
        let mut access = ResourceTable::new(
            "Access paths",
            CountLabel::number(preview.access_paths.len()),
            vec![
                Column::new("Filesystem", Priority::Identity, WidthPolicy::Auto),
                Column::new("Environment", Priority::Essential, WidthPolicy::Auto),
                Column::new("Path", Priority::Essential, WidthPolicy::Path),
                Column::new("State", Priority::Secondary, WidthPolicy::Auto),
            ],
        );
        for path in &preview.access_paths {
            let path_state = match path.state {
                crate::inventory::AccessState::Available => {
                    StateToken::positive(path.state.label())
                },
                crate::inventory::AccessState::FrontendStopped
                | crate::inventory::AccessState::Offline => StateToken::neutral(path.state.label()),
                crate::inventory::AccessState::Failed => StateToken::failure(path.state.label()),
            };
            access.push(ResourceRow::new(
                [
                    Cell::new(path.filesystem.label()),
                    Cell::new(path.environment.label()),
                    Cell::new(path.path.display().to_string()),
                    Cell::state(path_state),
                ],
                StateToken::neutral(path.state.label()),
            ));
        }
        report.push(Block::Resources(access));
    }
    report.render()
}

fn format_candidate(candidate: &CandidateSummary) -> String {
    candidate.version.as_ref().map_or_else(
        || format!("{}@{}", candidate.provider, candidate.id),
        |version| format!("{}@{version}", candidate.provider),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use omnifs_workspace::config::{Environment, Filesystem};

    fn candidate(provider: &str, version: Option<&str>, byte: u8) -> Candidate {
        Candidate {
            id: ProviderId::from_wasm_bytes(&[byte]),
            provider: provider.to_owned(),
            version: version.map(str::to_owned),
        }
    }

    fn preview(changed: bool) -> UpgradePreview {
        UpgradePreview {
            mount: "github".to_owned(),
            before: CandidateSummary {
                id: "blake3:before".to_owned(),
                provider: "github".to_owned(),
                version: Some("1.0.0".to_owned()),
            },
            after: CandidateSummary {
                id: "blake3:after".to_owned(),
                provider: "github".to_owned(),
                version: Some("1.1.0".to_owned()),
            },
            delta: vec!["scopes +1".to_owned()],
            changed,
            access_paths: vec![AccessPath {
                filesystem: Filesystem::Fuse,
                environment: Environment::Host,
                path: PathBuf::from("/mnt/github"),
                state: crate::inventory::AccessState::Available,
            }],
        }
    }

    #[test]
    fn human_upgrade_review_uses_resource_table_and_attention_state() {
        let rendered = render_upgrade_review(&preview(true));
        assert!(rendered.contains("Provider change  1"));
        assert!(rendered.contains("Mount"));
        assert!(rendered.contains("From"));
        assert!(rendered.contains("To"));
        assert!(rendered.contains("Authority"));
        assert!(rendered.contains("State"));
        assert!(rendered.contains("review required"));
        assert!(rendered.contains("github@1.0.0"));
        assert!(rendered.contains("github@1.1.0"));
        assert!(rendered.contains("/mnt/github"));
        assert!(!rendered.contains("  before  "));
    }

    #[test]
    fn human_upgrade_receipt_reports_settled_state_and_access_path() {
        let rendered = render_upgrade_receipt(&preview(true));
        assert!(rendered.contains("updated"));
        assert!(rendered.contains("Access paths  1"));
        assert!(rendered.contains("/mnt/github"));
    }

    #[test]
    fn human_noop_receipt_is_neutral() {
        let rendered = render_upgrade_receipt(&preview(false));
        assert!(rendered.contains("unchanged"));
    }

    #[test]
    fn explicit_same_digest_is_noop() {
        let current = candidate("demo", Some("1.0.0"), 1);
        assert_eq!(
            select_candidate(
                &current,
                std::slice::from_ref(&current),
                Some(&current.id.to_string())
            ),
            Ok(Selection::Noop)
        );
    }

    #[test]
    fn implicit_selects_unique_highest_semver() {
        let current = candidate("demo", Some("1.0.0"), 1);
        let candidate = candidate("demo", Some("1.2.0"), 2);
        assert_eq!(
            select_candidate(&current, &[current.clone(), candidate.clone()], None),
            Ok(Selection::Candidate(candidate.id))
        );
    }

    #[test]
    fn non_semver_current_requires_explicit_target() {
        let current = candidate("demo", Some("dev"), 1);
        let candidate = candidate("demo", Some("1.2.0"), 2);
        assert!(matches!(
            select_candidate(&current, &[candidate], None),
            Err(SelectionError::CurrentNotSemver)
        ));
    }

    #[test]
    fn duplicate_highest_requires_digest() {
        let current = candidate("demo", Some("1.0.0"), 1);
        let a = candidate("demo", Some("2.0.0"), 2);
        let b = candidate("demo", Some("2.0.0"), 3);
        assert!(matches!(
            select_candidate(&current, &[a, b], None),
            Err(SelectionError::AmbiguousHighest { .. })
        ));
    }

    #[test]
    fn wrong_provider_and_missing_target_are_distinct() {
        let current = candidate("demo", Some("1.0.0"), 1);
        let other = candidate("other", Some("2.0.0"), 2);
        assert!(matches!(
            select_candidate(&current, std::slice::from_ref(&other), Some("2.0.0")),
            Err(SelectionError::WrongProvider { .. })
        ));
        assert!(matches!(
            select_candidate(&current, &[], Some("9.9.9")),
            Err(SelectionError::MissingVersion(_))
        ));
    }
}
