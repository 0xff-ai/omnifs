//! Status report: data types, collection, and rendering.

use crate::error::ExitCode;
use crate::inventory::{
    DaemonState, FrontendStatus, Inventory, MountStatus, ProviderState, ProviderStatus, Severity,
};
use crate::ui::table::{
    Action as TableAction, Block as TableBlock, Cell as TableCell, Column as TableColumn,
    ContextStrip as TableContext, Meta as TableMeta, Priority as TablePriority,
    Report as TableReport, ResourceRow as TableRow, ResourceTable as TableResources,
    StateToken as TableState, WidthPolicy as TableWidth,
};

/// Inventory-backed report used by status and bare omnifs. It intentionally
/// returns a human-only table or the serializable inventory, so rendering and
/// machine output cannot drift.
#[derive(Debug, Clone)]
pub(crate) struct InventoryReport {
    pub(crate) inventory: Inventory,
}

impl InventoryReport {
    pub(crate) async fn collect(workspace: &crate::workspace::Workspace) -> anyhow::Result<Self> {
        Ok(Self {
            inventory: Inventory::collect(workspace).await?,
        })
    }

    pub(crate) fn exit_code(&self) -> ExitCode {
        if self.inventory.daemon_state() == DaemonState::Unreachable {
            ExitCode::DaemonUnavailable
        } else {
            match self.inventory.verdict() {
                crate::inventory::Verdict::Ok => ExitCode::Success,
                crate::inventory::Verdict::Degraded => ExitCode::Degraded,
            }
        }
    }

    pub(crate) fn render(&self, detail: bool) -> TableReport {
        let mut report = TableReport::new();
        let daemon_state = self.inventory.daemon_state();
        let context_state = match daemon_state {
            DaemonState::Running => match self.inventory.verdict() {
                crate::inventory::Verdict::Ok => TableState::positive("healthy"),
                crate::inventory::Verdict::Degraded => TableState::attention("degraded"),
            },
            DaemonState::Starting => TableState::attention("starting"),
            DaemonState::Degraded => TableState::attention("degraded"),
            DaemonState::Stopped => TableState::neutral("stopped"),
            DaemonState::Unreachable => TableState::failure("unreachable"),
            DaemonState::Failed => TableState::failure("failed"),
        };
        let mut metadata = vec![
            TableMeta::new(
                "Daemon",
                self.inventory
                    .daemon
                    .pid()
                    .map_or_else(|| "stopped".to_owned(), |pid| pid.to_string()),
            ),
            TableMeta::new("namespace", "/"),
        ];
        if let Some(api) = self.inventory.daemon.api() {
            metadata.push(TableMeta::new(
                "API",
                format!("{}.{}", api.major, api.minor),
            ));
        }
        let mut context = TableContext::new(
            "omnifs",
            omnifs_workspace::layout::WorkspaceLayout::display(&self.inventory.home),
            context_state,
        )
        .with_metadata(metadata);
        match daemon_state {
            DaemonState::Stopped => {
                context = context.with_action(TableAction::fix("omnifs up"));
            },
            DaemonState::Failed | DaemonState::Unreachable => {
                context = context.with_action(TableAction::fix("omnifs logs"));
            },
            DaemonState::Starting | DaemonState::Running | DaemonState::Degraded => {},
        }
        report.push(TableBlock::Context(context));

        report.push(TableBlock::Resources(frontend_table(
            &self.inventory.frontends,
        )));

        report.push(TableBlock::Resources(mount_table(&self.inventory.mounts)));

        if detail {
            report.push(TableBlock::Resources(provider_table(
                &self.inventory.providers,
            )));
        }
        report
    }
}

fn provider_label(mount: &MountStatus) -> String {
    mount.provider.version.as_ref().map_or_else(
        || mount.provider.name.clone(),
        |version| format!("{}@{}", mount.provider.name, version),
    )
}

/// Shared table builders for list/show consumers. The report delegates to
/// these concrete schema owners, so callers cannot drift from status output.
pub(crate) fn frontend_table(frontends: &[FrontendStatus]) -> TableResources {
    let mut table = TableResources::new(
        "Frontends",
        frontends.len(),
        vec![
            TableColumn::new("Filesystem", TablePriority::Identity, TableWidth::Auto),
            TableColumn::new("Environment", TablePriority::Identity, TableWidth::Auto),
            TableColumn::new("Location", TablePriority::Essential, TableWidth::Path),
            TableColumn::new("Coverage", TablePriority::Secondary, TableWidth::Auto),
            TableColumn::new("State", TablePriority::Essential, TableWidth::Auto),
        ],
    );
    for frontend in frontends {
        let mut row = TableRow::new(
            [
                TableCell::new(frontend.filesystem.label()),
                TableCell::new(frontend.environment.label()),
                TableCell::new(
                    frontend
                        .location
                        .as_deref()
                        .map_or_else(|| "/omnifs".into(), |path| path.display().to_string()),
                ),
                TableCell::new(format!("all {} mounts", frontend.mount_count)),
                TableCell::state(table_state(
                    frontend.state.severity(),
                    frontend.state.label(),
                )),
            ],
            table_state(frontend.state.severity(), frontend.state.label()),
        );
        if let Some(fix) = &frontend.fix {
            row = row.with_action(TableAction::fix(fix.clone()));
        }
        table.push(row);
    }
    table
}

pub(crate) fn mount_table(mounts: &[MountStatus]) -> TableResources {
    let mut table = TableResources::new(
        "Mounts",
        mounts.len(),
        vec![
            TableColumn::new("Mount", TablePriority::Identity, TableWidth::Auto),
            TableColumn::new("Provider", TablePriority::Secondary, TableWidth::Auto),
            TableColumn::new("Auth", TablePriority::Essential, TableWidth::Auto),
            TableColumn::new("Serving", TablePriority::Essential, TableWidth::Auto),
            TableColumn::new("Access", TablePriority::Secondary, TableWidth::Auto),
        ],
    );
    for mount in mounts {
        let mut row = TableRow::new(
            [
                TableCell::new(format!("/{}", mount.name.trim_start_matches('/'))),
                TableCell::new(provider_label(mount)),
                TableCell::state(table_state(mount.auth.severity(), mount.auth.label())),
                TableCell::state(table_state(mount.serving.severity(), mount.serving.label())),
                TableCell::new(if mount.access_count == 0 {
                    "none".into()
                } else {
                    format!("{} frontends", mount.access_count)
                }),
            ],
            mount_row_state(mount),
        );
        if let Some(fix) = &mount.fix {
            row = row.with_action(TableAction::fix(fix.clone()));
        }
        table.push(row);
    }
    table
}

pub(crate) fn provider_table(providers: &[ProviderStatus]) -> TableResources {
    provider_rows_table("Providers", providers)
}

pub(crate) fn provider_rows_table(title: &str, rows: &[ProviderStatus]) -> TableResources {
    let missing = rows
        .iter()
        .filter(|provider| provider.state == ProviderState::Missing)
        .count();
    let count = if missing == 0 {
        crate::ui::table::CountLabel::named(rows.len(), "artifacts")
    } else {
        crate::ui::table::CountLabel::with_secondary(
            rows.len().saturating_sub(missing),
            "artifacts",
            missing,
            "missing",
        )
    };
    let mut table = TableResources::new(
        title,
        count,
        vec![
            TableColumn::new("Provider", TablePriority::Identity, TableWidth::Auto),
            TableColumn::new("Version", TablePriority::Essential, TableWidth::Auto),
            TableColumn::new("Artifact", TablePriority::Secondary, TableWidth::Digest),
            TableColumn::new("Mounts", TablePriority::Detail, TableWidth::Auto),
            TableColumn::new("State", TablePriority::Essential, TableWidth::Auto),
        ],
    );
    for provider in rows {
        let mut row = TableRow::new(
            [
                TableCell::new(provider.name.clone()),
                TableCell::new(
                    provider
                        .version
                        .clone()
                        .unwrap_or_else(|| "unversioned".into()),
                ),
                TableCell::new(provider.artifact.clone()),
                TableCell::new(provider.pinned_by.join(", ")),
                TableCell::state(table_state(
                    provider.state.severity(),
                    provider.state.label(),
                )),
            ],
            table_state(provider.state.severity(), provider.state.label()),
        );
        if let Some(fix) = &provider.fix {
            row = row.with_action(TableAction::fix(fix.clone()));
        }
        table.push(row);
    }
    table
}

fn mount_row_state(mount: &MountStatus) -> TableState {
    let severity = [mount.auth.severity(), mount.serving.severity()]
        .into_iter()
        .max_by_key(|severity| severity.rank())
        .unwrap_or(Severity::Neutral);
    let label = if mount.auth.severity().rank() >= mount.serving.severity().rank() {
        mount.auth.label()
    } else {
        mount.serving.label()
    };
    table_state(severity, label)
}

fn table_state(severity: Severity, label: impl Into<String>) -> TableState {
    match severity {
        Severity::Positive => TableState::positive(label),
        Severity::Neutral => TableState::neutral(label),
        Severity::Attention => TableState::attention(label),
        Severity::Error => TableState::failure(label),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::{ProviderState, ProviderStatus};

    fn report(daemon: DaemonState, degraded: bool) -> InventoryReport {
        let inventory = Inventory::test(
            daemon,
            Vec::new(),
            Vec::new(),
            if degraded {
                vec![ProviderStatus {
                    name: "missing".into(),
                    version: None,
                    artifact: "a".repeat(64),
                    pinned_by: Vec::new(),
                    state: ProviderState::Missing,
                    fix: Some("omnifs provider add <path>".into()),
                }]
            } else {
                Vec::new()
            },
        );
        InventoryReport { inventory }
    }

    #[test]
    fn status_exit_code_reserves_daemon_unreachable_for_code_three() {
        assert_eq!(
            report(DaemonState::Unreachable, true).exit_code(),
            ExitCode::DaemonUnavailable
        );
        assert_eq!(
            report(DaemonState::Running, false).exit_code(),
            ExitCode::Success
        );
        assert_eq!(
            report(DaemonState::Running, true).exit_code(),
            ExitCode::Degraded
        );
    }

    #[test]
    fn context_metadata_uses_namespace_identity_and_omits_unknown_api() {
        let rendered = report(DaemonState::Stopped, false)
            .render(false)
            .render_with(crate::ui::table::RenderOptions {
                width: 120,
                color: false,
            });
        assert!(rendered.contains("Daemon stopped · namespace /"));
        assert!(!rendered.contains("Namespace namespace"));
        assert!(!rendered.contains("API -"));
        assert!(rendered.contains("Fix  omnifs up"));
    }

    #[test]
    fn context_api_and_actions_follow_observed_daemon_state() {
        let mut healthy = report(DaemonState::Running, false);
        healthy.inventory.daemon.status.as_mut().unwrap().api_major = 7;
        let healthy_text = healthy
            .render(false)
            .render_with(crate::ui::table::RenderOptions {
                width: 120,
                color: false,
            });
        assert!(healthy_text.contains("API 7.0"));
        assert!(!healthy_text.contains("Fix  omnifs"));

        let unreachable = report(DaemonState::Unreachable, false)
            .render(false)
            .render_with(crate::ui::table::RenderOptions {
                width: 120,
                color: false,
            });
        assert!(unreachable.contains("× unreachable"));
        assert!(unreachable.contains("Fix  omnifs logs"));
    }
}
