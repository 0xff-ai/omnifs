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
    pub(crate) async fn collect(workspace: &omnifs_workspace::Workspace) -> anyhow::Result<Self> {
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
        let mut metadata = match self.inventory.daemon.pid() {
            Some(pid) => vec![
                TableMeta::new("daemon", format!("pid {pid}")),
                TableMeta::new("serving", count(self.inventory.mounts.len(), "mount")),
                TableMeta::new(
                    "",
                    count(attached_frontend_count(&self.inventory), "frontend"),
                ),
            ],
            None => vec![TableMeta::new(
                "",
                format!("{} configured", count(self.inventory.mounts.len(), "mount")),
            )],
        };
        if let Some(warmup) = &self.inventory.warmup
            && !warmup.is_complete()
        {
            metadata.push(TableMeta::new("provider warmup", warmup.summary()));
        }
        let mut context = TableContext::new(
            "omnifs",
            omnifs_workspace::display(&self.inventory.home),
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

/// A counted noun that agrees in number (`1 mount`, `3 mounts`): the context
/// strip and access cells read as prose, so a lone resource must not claim a
/// plural.
fn count(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("{n} {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

fn attached_frontend_count(inventory: &Inventory) -> usize {
    inventory
        .frontends
        .iter()
        .filter(|frontend| {
            matches!(
                frontend.state,
                crate::inventory::FrontendState::Attached
                    | crate::inventory::FrontendState::Running
            )
        })
        .count()
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
                TableCell::new(format!("all {}", count(frontend.mount_count, "mount"))),
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
                    count(mount.access_count, "frontend")
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

/// One honest headline label per explicit precedence: a provider error
/// outranks an auth needing attention, which outranks the serving state
/// itself (healthy or not). This is a fixed priority order, never a generic
/// "most severe of three" tie-break: a merely-informational `Severity`
/// (auth `not needed` is `Neutral`, the same rank as `stopped`) must never
/// beat a genuinely live mount's serving state just because it sorts
/// alongside a higher-severity row elsewhere.
pub(crate) fn mount_row_state(mount: &MountStatus) -> TableState {
    if mount.provider.state.severity() == Severity::Error {
        return table_state(Severity::Error, mount.provider.state.label());
    }
    if mount.auth.severity() >= Severity::Attention {
        return table_state(mount.auth.severity(), mount.auth.label());
    }
    table_state(mount.serving.severity(), mount.serving.label())
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
    fn stopped_context_metadata_names_configured_mounts_not_a_stale_pid() {
        let rendered =
            report(DaemonState::Stopped)
                .render()
                .render_with(crate::ui::table::RenderOptions {
                    width: 120,
                    color: false,
                });
        assert!(rendered.contains("0 mounts configured"), "{rendered}");
        assert!(rendered.contains("fix:  omnifs up"));
    }

    #[test]
    fn running_context_metadata_reports_pid_mounts_and_frontends_as_one_sentence() {
        let inventory = Inventory::test(
            DaemonState::Running,
            vec![crate::inventory::FrontendStatus {
                filesystem: crate::commands::frontend::FrontendFilesystem::Nfs,
                runtime: crate::commands::frontend::FrontendRuntime::Host,
                location: Some("/Users/raul/omnifs".into()),
                state: crate::inventory::FrontendState::Attached,
                scope: "all",
                mount_count: 1,
                fix: None,
            }],
            Vec::new(),
        );
        let rendered =
            InventoryReport { inventory }
                .render()
                .render_with(crate::ui::table::RenderOptions {
                    width: 120,
                    color: false,
                });
        assert!(
            rendered.contains("daemon pid 1, serving 0 mounts, 1 frontend"),
            "{rendered}"
        );
    }

    /// Spec 3.10's full shape: context line, `Frontends` and `Mounts`
    /// sections, and a degraded mount row carrying its `fix:` line on the
    /// following line, full width, never truncated. (`Inventory::test`
    /// fixes the daemon pid at 1 rather than the spec's illustrative
    /// 31114; the row shapes below are asserted structurally, not against
    /// that placeholder digit.)
    #[test]
    fn status_report_matches_the_documented_shape_with_a_degraded_row() {
        let inventory = Inventory::test(
            DaemonState::Running,
            vec![crate::inventory::FrontendStatus {
                filesystem: crate::commands::frontend::FrontendFilesystem::Nfs,
                runtime: crate::commands::frontend::FrontendRuntime::Host,
                location: Some("/Users/raul/omnifs".into()),
                state: crate::inventory::FrontendState::Attached,
                scope: "all",
                mount_count: 2,
                fix: None,
            }],
            vec![
                MountStatus {
                    name: "github".into(),
                    root: "/github".into(),
                    provider: crate::inventory::ProviderPin {
                        name: "github".into(),
                        version: Some("0.3.2".into()),
                        artifact: "a".repeat(64),
                        state: crate::inventory::ProviderPinState::Available,
                    },
                    auth: crate::inventory::AuthState::Ready,
                    serving: crate::inventory::ServingState::Live,
                    access_count: 1,
                    fix: None,
                },
                MountStatus {
                    name: "linear".into(),
                    root: "/linear".into(),
                    provider: crate::inventory::ProviderPin {
                        name: "linear".into(),
                        version: Some("0.4.0".into()),
                        artifact: "b".repeat(64),
                        state: crate::inventory::ProviderPinState::Available,
                    },
                    auth: crate::inventory::AuthState::Expired {
                        command: "omnifs mount reauth linear".into(),
                    },
                    serving: crate::inventory::ServingState::Live,
                    access_count: 1,
                    fix: Some("omnifs mount reauth linear".into()),
                },
            ],
        );
        let rendered =
            InventoryReport { inventory }
                .render()
                .render_with(crate::ui::table::RenderOptions {
                    width: 120,
                    color: false,
                });

        let lines = rendered.lines().collect::<Vec<_>>();
        assert!(lines[0].starts_with("omnifs  "), "{rendered}");
        // The `linear` mount's expired auth makes this inventory genuinely
        // degraded, so the header state honestly reflects that rather than
        // spec 3.10's illustrative all-clear `● healthy`.
        assert!(lines[0].trim_end().ends_with("▲ degraded"), "{rendered}");
        assert!(
            lines[1].contains("daemon pid 1, serving 2 mounts, 1 frontend"),
            "{rendered}"
        );
        assert!(rendered.contains("Frontends"), "{rendered}");
        assert!(rendered.contains("Mounts"), "{rendered}");
        assert!(rendered.contains("github"), "{rendered}");
        assert!(rendered.contains("● live"), "{rendered}");

        // The degraded `linear` row headlines its own auth state (spec item
        // 4's precedence) and carries its fix on the following line.
        let linear_index = lines
            .iter()
            .position(|line| line.contains("linear"))
            .expect("linear row");
        assert!(lines[linear_index].contains("▲ expired"), "{rendered}");
        assert_eq!(
            lines[linear_index + 1].trim(),
            "fix:  omnifs mount reauth linear",
            "{rendered}"
        );
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
        assert!(!healthy_text.contains("fix:  omnifs"));

        let unreachable = report(DaemonState::Unreachable).render().render_with(
            crate::ui::table::RenderOptions {
                width: 120,
                color: false,
            },
        );
        assert!(unreachable.contains("× unreachable"));
        assert!(unreachable.contains("fix:  omnifs logs"));
    }

    /// Regression for the footgun this slice fixes: a live mount whose auth
    /// needs none (`Severity::Neutral`, same rank as `Serving::Stopped`)
    /// must headline as `live`, never lose to the merely-informational
    /// `not needed` auth label through a generic "most severe" tie-break.
    #[test]
    fn live_mount_headlines_serving_state_not_a_neutral_auth_label() {
        let mount = MountStatus {
            name: "dns".into(),
            root: "/dns".into(),
            provider: crate::inventory::ProviderPin {
                name: "dns".into(),
                version: Some("0.2.1".into()),
                artifact: "a".repeat(64),
                state: crate::inventory::ProviderPinState::Available,
            },
            auth: crate::inventory::AuthState::NotNeeded,
            serving: crate::inventory::ServingState::Live,
            access_count: 0,
            fix: None,
        };
        let state = mount_row_state(&mount);
        let rendered = format!("{state:?}");
        assert!(rendered.contains("live"), "{rendered}");
        assert!(!rendered.contains("not needed"), "{rendered}");
    }

    #[test]
    fn provider_error_outranks_a_live_serving_state() {
        let mount = MountStatus {
            name: "github".into(),
            root: "/github".into(),
            provider: crate::inventory::ProviderPin {
                name: "github".into(),
                version: None,
                artifact: "a".repeat(64),
                state: crate::inventory::ProviderPinState::Corrupt {
                    message: "digest mismatch".into(),
                },
            },
            auth: crate::inventory::AuthState::Ready,
            serving: crate::inventory::ServingState::Live,
            access_count: 0,
            fix: None,
        };
        let rendered = format!("{:?}", mount_row_state(&mount));
        assert!(rendered.contains("corrupt"), "{rendered}");
    }

    #[test]
    fn auth_needing_attention_outranks_a_stopped_serving_state() {
        let mount = MountStatus {
            name: "github".into(),
            root: "/github".into(),
            provider: crate::inventory::ProviderPin {
                name: "github".into(),
                version: None,
                artifact: "a".repeat(64),
                state: crate::inventory::ProviderPinState::Available,
            },
            auth: crate::inventory::AuthState::Expired {
                command: "omnifs mount reauth github".into(),
            },
            serving: crate::inventory::ServingState::Stopped,
            access_count: 0,
            fix: None,
        };
        let rendered = format!("{:?}", mount_row_state(&mount));
        assert!(rendered.contains("expired"), "{rendered}");
    }
}
