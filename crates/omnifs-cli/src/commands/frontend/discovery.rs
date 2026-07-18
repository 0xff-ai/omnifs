//! Frontend support discovery and listing.

use clap::Args;
use serde::Serialize;

use super::lifecycle::{FrontendFilesystem, FrontendRuntime};
use crate::docker::DockerClient;
use crate::host_runner::{HostRunner, LocalProtocol};
use crate::inventory::{FrontendStatus, Inventory};
use crate::libkrun_runner::LibkrunRunner;
use crate::ui::output::{Output, ResultVerdict};
use omnifs_workspace::Workspace;

#[derive(Args, Debug, Clone, Default)]
pub(crate) struct FrontendLsArgs {}

impl FrontendFilesystem {
    const ALL: [Self; 2] = [Self::Fuse, Self::Nfs];

    /// `pub(crate)`, not `pub(super)`: `omnifs setup`'s frontend multi-select
    /// (`commands::setup`, a sibling of `commands::frontend`) reads this to
    /// pre-check the platform's recommended default alongside every other
    /// caller inside `commands::frontend` itself.
    pub(crate) const fn default_runtime(self) -> FrontendRuntime {
        match self {
            Self::Fuse if cfg!(target_os = "macos") => FrontendRuntime::Libkrun,
            Self::Fuse | Self::Nfs => FrontendRuntime::Host,
        }
    }
}

impl FrontendRuntime {
    const ALL: [Self; 3] = [Self::Host, Self::Docker, Self::Libkrun];

    pub(super) fn supports(self, filesystem: FrontendFilesystem) -> bool {
        Platform::current().supports(filesystem, self)
    }

    const fn instances(self) -> InstancePolicy {
        match self {
            Self::Host => InstancePolicy::MultipleLocations,
            Self::Docker | Self::Libkrun => InstancePolicy::OnePerWorkspace,
        }
    }
}

/// Every filesystem/runtime pair supported on this OS, in `FrontendFilesystem`
/// then `FrontendRuntime` enumeration order. The single owner of "which
/// frontends exist on this platform": `frontend ls`'s support table and
/// `omnifs setup`'s frontend multi-select both read this rather than each
/// re-deriving the platform table.
pub(crate) fn available_frontends() -> Vec<(FrontendFilesystem, FrontendRuntime)> {
    let platform = Platform::current();
    let mut out = Vec::new();
    for filesystem in FrontendFilesystem::ALL {
        for runtime in FrontendRuntime::ALL {
            if platform.supports(filesystem, runtime) {
                out.push((filesystem, runtime));
            }
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
struct Platform {
    os: &'static str,
    arch: &'static str,
}

impl Platform {
    const fn current() -> Self {
        Self {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
        }
    }

    fn supports(self, filesystem: FrontendFilesystem, runtime: FrontendRuntime) -> bool {
        matches!(
            (self.os, filesystem, runtime),
            (
                "macos",
                FrontendFilesystem::Fuse,
                FrontendRuntime::Docker | FrontendRuntime::Libkrun
            ) | ("macos", FrontendFilesystem::Nfs, FrontendRuntime::Host)
                | (
                    "linux",
                    FrontendFilesystem::Fuse,
                    FrontendRuntime::Host | FrontendRuntime::Docker
                )
        )
    }

    fn label(self) -> String {
        let os = match self.os {
            "macos" => "macOS",
            "linux" => "Linux",
            other => other,
        };
        format!("{os} {}", self.arch)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum InstancePolicy {
    MultipleLocations,
    OnePerWorkspace,
}

impl InstancePolicy {
    const fn label(self) -> &'static str {
        match self {
            Self::MultipleLocations => "multiple locations",
            Self::OnePerWorkspace => "one per workspace",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct FrontendSupport {
    filesystem: FrontendFilesystem,
    runtime: FrontendRuntime,
    default: bool,
    instances: InstancePolicy,
    available: bool,
    detail: String,
}

impl FrontendSupport {
    async fn inspect(filesystem: FrontendFilesystem, runtime: FrontendRuntime) -> Self {
        let default = filesystem.default_runtime() == runtime;
        let readiness = match runtime {
            FrontendRuntime::Host => HostRunner::probe(match filesystem {
                FrontendFilesystem::Fuse => LocalProtocol::Fuse,
                FrontendFilesystem::Nfs => LocalProtocol::Nfs,
            }),
            FrontendRuntime::Docker => DockerClient::probe().await,
            FrontendRuntime::Libkrun => LibkrunRunner::probe(),
        };
        let command = if default {
            format!("omnifs frontend enable {filesystem}")
        } else {
            format!("omnifs frontend enable {filesystem} --runtime {runtime}")
        };
        let (available, detail) = match readiness {
            Ok(()) => (true, command),
            Err(error) => (false, format!("{error:#}")),
        };
        Self {
            filesystem,
            runtime,
            default,
            instances: runtime.instances(),
            available,
            detail,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct FrontendList {
    platform: Platform,
    supported_frontends: Vec<FrontendSupport>,
    frontends: Vec<FrontendStatus>,
    verdict: crate::inventory::Verdict,
}

impl FrontendList {
    async fn collect(inventory: &Inventory) -> Self {
        let platform = Platform::current();
        let mut supported_frontends = Vec::new();
        for filesystem in FrontendFilesystem::ALL {
            for runtime in FrontendRuntime::ALL {
                if platform.supports(filesystem, runtime) {
                    supported_frontends.push(FrontendSupport::inspect(filesystem, runtime).await);
                }
            }
        }
        Self {
            platform,
            supported_frontends,
            frontends: inventory.frontends.clone(),
            verdict: inventory.verdict(),
        }
    }

    fn support_table(&self) -> crate::ui::table::ResourceTable {
        use crate::ui::table::{
            Cell, Column, Priority, ResourceRow, ResourceTable, StateToken, WidthPolicy,
        };

        let mut table = ResourceTable::new(
            format!("Supported frontends on {}", self.platform.label()),
            self.supported_frontends.len(),
            vec![
                Column::new("Filesystem", Priority::Identity, WidthPolicy::Auto),
                Column::new("Runtime", Priority::Identity, WidthPolicy::Auto),
                Column::new("Default", Priority::Detail, WidthPolicy::Auto),
                Column::new("Instances", Priority::Essential, WidthPolicy::Auto),
                Column::new("Availability", Priority::Essential, WidthPolicy::Auto),
                Column::new("Enable or reason", Priority::Essential, WidthPolicy::Path),
            ],
        );
        for support in &self.supported_frontends {
            let state = if support.available {
                StateToken::positive("available")
            } else {
                StateToken::neutral("unavailable")
            };
            table.push(ResourceRow::new(
                [
                    Cell::new(support.filesystem.label()),
                    Cell::new(support.runtime.label()),
                    Cell::new(if support.default { "yes" } else { "no" }),
                    Cell::new(support.instances.label()),
                    Cell::state(state.clone()),
                    Cell::new(&support.detail),
                ],
                state,
            ));
        }
        table
    }

    fn render(&self) -> crate::ui::table::Report {
        use crate::ui::table::{Block, Report};

        let mut report = Report::new();
        report.push(Block::Resources(self.support_table()));
        let mut instantiated = crate::status::frontend_table(&self.frontends);
        "Instantiated frontends".clone_into(&mut instantiated.title);
        report.push(Block::Resources(instantiated));
        report
    }
}

impl FrontendLsArgs {
    pub(crate) async fn run(self, output: Output) -> anyhow::Result<crate::error::ExitCode> {
        let workspace = Workspace::resolve()?;
        let inventory = Inventory::collect(&workspace).await?;
        let list = FrontendList::collect(&inventory).await;
        let exit = if inventory.verdict() == crate::inventory::Verdict::Degraded {
            crate::error::ExitCode::Degraded
        } else {
            crate::error::ExitCode::Success
        };
        if output.is_structured() {
            output.emit_result(ResultVerdict::from(inventory.verdict()), &list)?;
        } else {
            list.render().print();
        }
        Ok(exit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontend_policy_matches_supported_platforms() {
        let macos = Platform {
            os: "macos",
            arch: "aarch64",
        };
        assert!(macos.supports(FrontendFilesystem::Fuse, FrontendRuntime::Libkrun));
        assert!(macos.supports(FrontendFilesystem::Fuse, FrontendRuntime::Docker));
        assert!(macos.supports(FrontendFilesystem::Nfs, FrontendRuntime::Host));
        assert!(!macos.supports(FrontendFilesystem::Fuse, FrontendRuntime::Host));

        let linux = Platform {
            os: "linux",
            arch: "x86_64",
        };
        assert!(linux.supports(FrontendFilesystem::Fuse, FrontendRuntime::Host));
        assert!(linux.supports(FrontendFilesystem::Fuse, FrontendRuntime::Docker));
        assert!(!linux.supports(FrontendFilesystem::Nfs, FrontendRuntime::Host));

        assert_eq!(
            FrontendFilesystem::Nfs.default_runtime(),
            FrontendRuntime::Host
        );
        assert_eq!(
            FrontendFilesystem::Fuse.default_runtime(),
            if cfg!(target_os = "macos") {
                FrontendRuntime::Libkrun
            } else {
                FrontendRuntime::Host
            }
        );
    }
}
