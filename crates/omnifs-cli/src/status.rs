//! Status report: data types, collection, and rendering.

use crate::error::ExitCode;
use crate::inventory::{DaemonState, FrontendStatus, Inventory, MountStatus, Severity};
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

    pub(crate) fn render(&self) -> TableReport {
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
        if let Some(warmup) = &self.inventory.warmup {
            metadata.push(TableMeta::new("provider warmup", warmup.summary()));
        }
        let mut context = TableContext::new(
            "omnifs",
            omnifs_workspace::layout::display(&self.inventory.home),
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

        report
    }
}

fn provider_label(mount: &MountStatus) -> String {
    let identity = mount.provider.version.as_ref().map_or_else(
        || mount.provider.name.clone(),
        |version| format!("{}@{}", mount.provider.name, version),
    );
    format!("{identity} ({})", mount.provider.state.label())
}

/// Shared table builders for list/show consumers. The report delegates to
/// these concrete schema owners, so callers cannot drift from status output.
pub(crate) fn frontend_table(frontends: &[FrontendStatus]) -> TableResources {
    let mut table = TableResources::new(
        "Frontends",
        frontends.len(),
        vec![
            TableColumn::new("Filesystem", TablePriority::Identity, TableWidth::Auto),
            TableColumn::new("Runtime", TablePriority::Identity, TableWidth::Auto),
            TableColumn::new("Location", TablePriority::Essential, TableWidth::Path),
            TableColumn::new("Coverage", TablePriority::Secondary, TableWidth::Auto),
            TableColumn::new("State", TablePriority::Essential, TableWidth::Auto),
        ],
    );
    for frontend in frontends {
        let mut row = TableRow::new(
            [
                TableCell::new(frontend.filesystem.label()),
                TableCell::new(frontend.runtime.label()),
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

fn mount_row_state(mount: &MountStatus) -> TableState {
    let severity = [
        mount.provider.state.severity(),
        mount.auth.severity(),
        mount.serving.severity(),
    ]
    .into_iter()
    .max_by_key(|severity| severity.rank())
    .unwrap_or(Severity::Neutral);
    let label = if mount.provider.state.severity().rank() >= mount.auth.severity().rank()
        && mount.provider.state.severity().rank() >= mount.serving.severity().rank()
    {
        mount.provider.state.label()
    } else if mount.auth.severity().rank() >= mount.serving.severity().rank() {
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

    fn report(daemon: DaemonState) -> InventoryReport {
        let inventory = Inventory::test(daemon, Vec::new(), Vec::new());
        InventoryReport { inventory }
    }

    #[test]
    fn status_exit_code_reserves_daemon_unreachable_for_code_three() {
        assert_eq!(
            report(DaemonState::Unreachable).exit_code(),
            ExitCode::DaemonUnavailable
        );
        assert_eq!(report(DaemonState::Running).exit_code(), ExitCode::Success);
    }

    #[test]
    fn context_metadata_uses_namespace_identity() {
        let rendered =
            report(DaemonState::Stopped)
                .render()
                .render_with(crate::ui::table::RenderOptions {
                    width: 120,
                    color: false,
                });
        assert!(rendered.contains("Daemon stopped · namespace /"));
        assert!(!rendered.contains("Namespace namespace"));
        assert!(rendered.contains("Fix  omnifs up"));
    }

    #[test]
    fn context_actions_follow_observed_daemon_state() {
        let healthy = report(DaemonState::Running);
        let healthy_text = healthy
            .render()
            .render_with(crate::ui::table::RenderOptions {
                width: 120,
                color: false,
            });
        assert!(!healthy_text.contains("Fix  omnifs"));

        let unreachable = report(DaemonState::Unreachable).render().render_with(
            crate::ui::table::RenderOptions {
                width: 120,
                color: false,
            },
        );
        assert!(unreachable.contains("× unreachable"));
        assert!(unreachable.contains("Fix  omnifs logs"));
    }
}
