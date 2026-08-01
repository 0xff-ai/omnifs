//! Status report: data types, collection, and rendering.

use crate::error::ExitCode;
use crate::inventory::{
    ActionTarget, DaemonHealth, FilesystemAccessState, FilesystemAccessStatus, Inventory,
    MountStatus, NextAction, ServingState, Severity,
};
use crate::ui::output::ResultVerdict;
use crate::ui::render::count;
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
    pub(crate) async fn collect() -> anyhow::Result<Self> {
        Ok(Self {
            inventory: Inventory::collect_rpc().await?,
        })
    }

    pub(crate) fn exit_code(&self) -> ExitCode {
        if self.inventory.daemon_health() == DaemonHealth::Unreachable {
            ExitCode::DaemonUnavailable
        } else {
            match self.inventory.verdict() {
                ResultVerdict::Ok => ExitCode::Success,
                ResultVerdict::Degraded => ExitCode::Degraded,
            }
        }
    }

    pub(crate) fn render(&self) -> TableReport {
        let mut report = TableReport::new();
        let daemon_health = self.inventory.daemon_health();
        let next_action = self.inventory.next_action();
        let context_state = if daemon_health == DaemonHealth::Running {
            match self.inventory.verdict() {
                ResultVerdict::Ok => TableState::positive("healthy"),
                ResultVerdict::Degraded => TableState::attention("degraded"),
            }
        } else {
            let (severity, label) = daemon_health.descriptor();
            TableState::new(severity.into(), label)
        };
        let metadata = match self.inventory.daemon.pid() {
            Some(pid) => vec![
                TableMeta::new("daemon", format!("pid {pid}")),
                TableMeta::new("serving", count(self.inventory.mounts.len(), "mount")),
                TableMeta::new(
                    "",
                    count(self.inventory.ready_filesystem_count(), "filesystem"),
                ),
            ],
            None => vec![TableMeta::new(
                "",
                format!("{} configured", count(self.inventory.mounts.len(), "mount")),
            )],
        };
        let mut context = TableContext::new(
            "omnifs",
            self.inventory.home.display().to_string(),
            context_state,
        )
        .with_metadata(metadata);
        if matches!(
            next_action,
            Some(NextAction::Doctor {
                target: ActionTarget::Profile
            })
        ) {
            context = context.with_action(TableAction::fix("omnifs doctor"));
        }
        report.push(TableBlock::Context(context));

        report.push(TableBlock::Resources(mount_table(
            &self.inventory.mounts,
            self.inventory.primary_host_location(),
            next_action.as_ref(),
        )));

        let clean_stopped_filesystems = daemon_health == DaemonHealth::Stopped
            && self
                .inventory
                .filesystems
                .iter()
                .all(|filesystem| filesystem.state == FilesystemAccessState::Stopped);
        if !clean_stopped_filesystems {
            report.push(TableBlock::Resources(filesystem_table(
                &self.inventory.filesystems,
                next_action.as_ref(),
            )));
        }

        report
    }

    pub(crate) fn closing_action(&self) -> Option<NextAction> {
        self.inventory.next_action().filter(|action| {
            matches!(
                action,
                NextAction::WaitForFilesystem { .. }
                    | NextAction::CreateFilesystem
                    | NextAction::Browse { .. }
                    | NextAction::EnterFilesystem { .. }
            )
        })
    }
}

/// Shared table builders for list/show consumers. The report delegates to
/// these concrete schema owners, so callers cannot drift from status output.
pub(crate) fn filesystem_table(
    filesystems: &[FilesystemAccessStatus],
    next_action: Option<&NextAction>,
) -> TableResources {
    let mut table = TableResources::new(
        "Filesystems",
        filesystem_summary(filesystems),
        vec![
            TableColumn::new("Name", TablePriority::Identity, TableWidth::Auto),
            TableColumn::new("Protocol", TablePriority::Identity, TableWidth::Auto),
            TableColumn::new("Runtime", TablePriority::Identity, TableWidth::Auto),
            TableColumn::new("Location", TablePriority::Essential, TableWidth::Path),
            TableColumn::new("Coverage", TablePriority::Secondary, TableWidth::Auto),
            TableColumn::new("State", TablePriority::Essential, TableWidth::Auto),
        ],
    );
    for filesystem in filesystems {
        let mut row = TableRow::new(
            [
                TableCell::new(filesystem.name.as_str()),
                TableCell::new(filesystem.spec.protocol().as_str()),
                TableCell::new(filesystem.spec.runtime().as_str()),
                TableCell::new(filesystem.spec.location().display().to_string()),
                TableCell::new(format!("all {}", count(filesystem.mount_count, "mount"))),
                TableCell::state(TableState::new(
                    filesystem.state.severity().into(),
                    filesystem.state.label(),
                )),
            ],
            TableState::new(filesystem.state.severity().into(), filesystem.state.label()),
        );
        if matches!(
            next_action,
            Some(NextAction::Doctor {
                target: ActionTarget::Filesystem(id)
            }) if id == &filesystem.name
        ) {
            row = row.with_action(TableAction::fix("omnifs doctor"));
        }
        table.push(row);
    }
    table
}

pub(crate) fn mount_table(
    mounts: &[MountStatus],
    host_location: Option<&std::path::Path>,
    next_action: Option<&NextAction>,
) -> TableResources {
    let mut table = TableResources::new(
        "Mounts",
        mount_summary(mounts),
        vec![
            TableColumn::new("Mount", TablePriority::Identity, TableWidth::Auto),
            TableColumn::new("Provider", TablePriority::Secondary, TableWidth::Auto),
            TableColumn::new("Auth", TablePriority::Essential, TableWidth::Auto),
            TableColumn::new("Serving", TablePriority::Essential, TableWidth::Auto),
            TableColumn::new("Files at", TablePriority::Secondary, TableWidth::Path),
        ],
    );
    for mount in mounts {
        let mut row = TableRow::new(
            [
                TableCell::new(format!("/{}", mount.name.trim_start_matches('/'))),
                TableCell::new(mount.provider.to_string()),
                TableCell::state(TableState::new(
                    mount.auth.severity().into(),
                    mount.auth.label(),
                )),
                TableCell::state(TableState::new(
                    mount.serving.severity().into(),
                    mount.serving.label(),
                )),
                TableCell::new(mount.access_path(host_location)),
            ],
            mount_row_state(mount),
        );
        let action = match next_action {
            Some(NextAction::Doctor {
                target: ActionTarget::Mount(name),
            }) if name == &mount.name => Some("omnifs doctor".to_owned()),
            Some(NextAction::Reauthenticate { mount: name }) if name == &mount.name => {
                Some(format!("omnifs mount reauth {}", mount.name))
            },
            _ => None,
        };
        if let Some(action) = action {
            row = row.with_action(TableAction::fix(action));
        }
        table.push(row);
    }
    table
}

fn filesystem_summary(filesystems: &[FilesystemAccessStatus]) -> String {
    if filesystems.is_empty() {
        return "none configured".to_owned();
    }
    let count_state = |state| {
        filesystems
            .iter()
            .filter(|filesystem| filesystem.state == state)
            .count()
    };
    let parts = [
        FilesystemAccessState::Ready,
        FilesystemAccessState::Stopped,
        FilesystemAccessState::Unknown,
        FilesystemAccessState::Failed,
    ]
    .into_iter()
    .map(|state| (count_state(state), state.label()))
    .filter(|(count, _)| *count > 0)
    .map(|(count, label)| format!("{count} {label}"))
    .collect::<Vec<_>>();
    if parts.len() == 1 {
        parts[0].clone()
    } else {
        format!("{} configured, {}", filesystems.len(), parts.join(", "))
    }
}

fn mount_summary(mounts: &[MountStatus]) -> String {
    if mounts.is_empty() {
        return "none configured".to_owned();
    }
    let live = mounts
        .iter()
        .filter(|mount| mount.serving == ServingState::Live)
        .count();
    let needs_attention = mounts
        .iter()
        .filter(|mount| mount.needs_attention())
        .count();
    if needs_attention > 0 {
        let mut parts = Vec::new();
        if live > 0 {
            parts.push(format!("{live} live"));
        }
        parts.push(format!("{needs_attention} needs attention"));
        return format!("{} configured, {}", mounts.len(), parts.join(", "));
    }
    if live == mounts.len() {
        return format!("{live} live");
    }
    if mounts
        .iter()
        .all(|mount| mount.serving == ServingState::Stopped)
    {
        return format!("{} configured, stopped", mounts.len());
    }
    format!("{} configured", mounts.len())
}

/// The row's headline state: `MountStatus::headline` owns the precedence
/// (provider pin outranks auth, which outranks serving), this just converts
/// it to the table's own severity vocabulary.
pub(crate) fn mount_row_state(mount: &MountStatus) -> TableState {
    let (severity, label) = mount.headline();
    TableState::new(severity.into(), label)
}

impl From<Severity> for crate::ui::table::Severity {
    fn from(severity: Severity) -> Self {
        match severity {
            Severity::Positive => Self::Positive,
            Severity::Neutral => Self::Neutral,
            Severity::Attention => Self::Attention,
            Severity::Failure => Self::Failure,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(daemon: DaemonHealth) -> InventoryReport {
        let inventory = Inventory::test(daemon, Vec::new(), Vec::new());
        InventoryReport { inventory }
    }

    #[test]
    fn status_exit_code_reserves_daemon_unreachable_for_code_three() {
        assert_eq!(
            report(DaemonHealth::Unreachable).exit_code(),
            ExitCode::DaemonUnavailable
        );
        assert_eq!(report(DaemonHealth::Running).exit_code(), ExitCode::Success);
    }

    #[test]
    fn empty_stopped_context_names_configured_mounts_without_a_false_start_action() {
        let rendered =
            report(DaemonHealth::Stopped)
                .render()
                .render_with(crate::ui::table::RenderOptions {
                    width: 120,
                    color: false,
                });
        assert!(rendered.contains("0 mounts configured"), "{rendered}");
        assert!(!rendered.contains("fix:"), "{rendered}");
    }

    #[test]
    fn running_context_metadata_reports_pid_mounts_and_filesystems_as_one_sentence() {
        let inventory = Inventory::test(
            DaemonHealth::Running,
            vec![crate::inventory::FilesystemAccessStatus {
                name: "host".parse().unwrap(),
                spec: omnifs_core::FilesystemSpec::new(
                    omnifs_core::FilesystemProtocol::Nfs,
                    omnifs_core::FilesystemRuntime::Host,
                    "/Users/raul/omnifs".into(),
                    None,
                    None,
                )
                .unwrap(),
                state: crate::inventory::FilesystemAccessState::Ready,
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
            rendered.contains("daemon pid 1, serving 0 mounts, 1 filesystem"),
            "{rendered}"
        );
    }

    /// The full shape: context line, `Mounts` and `Filesystems`
    /// sections, and a degraded mount row carrying its `fix:` line on the
    /// following line, full width, never truncated. (`Inventory::test`
    /// fixes the daemon pid at 1 rather than the illustrative
    /// 31114; the row shapes below are asserted structurally, not against
    /// that placeholder digit.)
    #[test]
    fn status_report_matches_the_documented_shape_with_a_degraded_row() {
        let inventory = Inventory::test(
            DaemonHealth::Running,
            vec![crate::inventory::FilesystemAccessStatus {
                name: "host".parse().unwrap(),
                spec: omnifs_core::FilesystemSpec::new(
                    omnifs_core::FilesystemProtocol::Nfs,
                    omnifs_core::FilesystemRuntime::Host,
                    "/Users/raul/omnifs".into(),
                    None,
                    None,
                )
                .unwrap(),
                state: crate::inventory::FilesystemAccessState::Ready,
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
        // the illustrative all-clear `● healthy`.
        assert!(lines[0].trim_end().ends_with("▲ degraded"), "{rendered}");
        assert!(
            lines[1].contains("daemon pid 1, serving 2 mounts, 1 filesystem"),
            "{rendered}"
        );
        assert!(rendered.contains("Filesystems"), "{rendered}");
        assert!(rendered.contains("Mounts"), "{rendered}");
        assert!(
            rendered.find("Mounts").unwrap() < rendered.find("Filesystems").unwrap(),
            "{rendered}"
        );
        assert!(rendered.contains("github"), "{rendered}");
        assert!(rendered.contains("● live"), "{rendered}");

        // The degraded `linear` row headlines its own auth state and carries
        // its fix on the following line.
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
    fn context_actions_follow_observed_daemon_health() {
        let healthy = report(DaemonHealth::Running);
        let healthy_text = healthy
            .render()
            .render_with(crate::ui::table::RenderOptions {
                width: 120,
                color: false,
            });
        assert!(!healthy_text.contains("fix:  omnifs"));

        let unreachable = report(DaemonHealth::Unreachable).render().render_with(
            crate::ui::table::RenderOptions {
                width: 120,
                color: false,
            },
        );
        assert!(unreachable.contains("× unreachable"));
        assert!(unreachable.contains("fix:  omnifs doctor"));
    }

    #[test]
    fn clean_stopped_status_hides_stopped_filesystem_rows() {
        let inventory = Inventory::test(
            DaemonHealth::Stopped,
            vec![crate::inventory::FilesystemAccessStatus {
                name: "host".parse().unwrap(),
                spec: omnifs_core::FilesystemSpec::new(
                    omnifs_core::FilesystemProtocol::Nfs,
                    omnifs_core::FilesystemRuntime::Host,
                    "/Users/raul/omnifs".into(),
                    None,
                    None,
                )
                .unwrap(),
                state: crate::inventory::FilesystemAccessState::Stopped,
                mount_count: 0,
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
        assert!(!rendered.contains("Filesystems"), "{rendered}");
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
        };
        let rendered = format!("{:?}", mount_row_state(&mount));
        assert!(rendered.contains("expired"), "{rendered}");
    }
}
