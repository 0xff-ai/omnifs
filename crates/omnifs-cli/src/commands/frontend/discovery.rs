//! Frontend support discovery and listing.

use omnifs_api::{FrontendRuntime, FsType};
use serde::Serialize;

use crate::docker::DockerClient;
use crate::host_runner::HostRunner;
use crate::inventory::{FrontendStatus, Inventory};
use crate::libkrun_runner::LibkrunRunner;
use crate::ui::output::{Output, ResultVerdict};
use omnifs_workspace::Workspace;

const FS_TYPES: [FsType; 2] = [FsType::Fuse, FsType::Nfs];
const RUNTIMES: [FrontendRuntime; 3] = [
    FrontendRuntime::Host,
    FrontendRuntime::Docker,
    FrontendRuntime::Libkrun,
];

/// The platform's recommended runtime for one filesystem.
pub(crate) fn default_runtime(filesystem: FsType) -> Option<FrontendRuntime> {
    Platform::current().default_runtime(filesystem)
}

pub(crate) fn supports(filesystem: FsType, runtime: FrontendRuntime) -> bool {
    Platform::current().supports(filesystem, runtime)
}

/// Every filesystem/runtime pair supported on this OS, in `FsType`
/// then `FrontendRuntime` enumeration order. The single owner of "which
/// frontends exist on this platform": `frontend ls`'s support table and
/// `omnifs setup`'s frontend multi-select both read this rather than each
/// re-deriving the platform table.
pub(crate) fn available_frontends() -> Vec<(FsType, FrontendRuntime)> {
    let platform = Platform::current();
    let mut out = Vec::new();
    for filesystem in FS_TYPES {
        for runtime in RUNTIMES {
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

    fn supports(self, filesystem: FsType, runtime: FrontendRuntime) -> bool {
        match (self.os, filesystem, runtime) {
            ("macos", FsType::Fuse, FrontendRuntime::Libkrun) => self.arch == "aarch64",
            ("macos", FsType::Fuse, FrontendRuntime::Docker)
            | ("macos" | "linux", FsType::Nfs, FrontendRuntime::Host)
            | ("linux", FsType::Fuse, FrontendRuntime::Host | FrontendRuntime::Docker) => true,
            _ => false,
        }
    }

    fn default_runtime(self, filesystem: FsType) -> Option<FrontendRuntime> {
        match (self.os, self.arch, filesystem) {
            ("macos", "aarch64", FsType::Fuse) => Some(FrontendRuntime::Libkrun),
            ("macos" | "linux", _, FsType::Nfs) | ("linux", _, FsType::Fuse) => {
                Some(FrontendRuntime::Host)
            },
            _ => None,
        }
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

const fn instance_policy(runtime: FrontendRuntime) -> InstancePolicy {
    match runtime {
        FrontendRuntime::Host => InstancePolicy::MultipleLocations,
        FrontendRuntime::Docker | FrontendRuntime::Libkrun => InstancePolicy::OnePerWorkspace,
    }
}

#[derive(Debug, Clone, Serialize)]
struct FrontendSupport {
    filesystem: FsType,
    runtime: FrontendRuntime,
    default: bool,
    instances: InstancePolicy,
    available: bool,
    detail: String,
}

impl FrontendSupport {
    async fn inspect(filesystem: FsType, runtime: FrontendRuntime) -> Self {
        let default = default_runtime(filesystem) == Some(runtime);
        let readiness = match runtime {
            FrontendRuntime::Host => HostRunner::probe(filesystem),
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
            instances: instance_policy(runtime),
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
        for (filesystem, runtime) in available_frontends() {
            supported_frontends.push(FrontendSupport::inspect(filesystem, runtime).await);
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
                    Cell::new(support.filesystem.as_str()),
                    Cell::new(support.runtime.as_str()),
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

pub(crate) async fn run(output: Output) -> anyhow::Result<crate::error::ExitCode> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontend_policy_matches_supported_platforms() {
        let macos = Platform {
            os: "macos",
            arch: "aarch64",
        };
        assert!(macos.supports(FsType::Fuse, FrontendRuntime::Libkrun));
        assert!(macos.supports(FsType::Fuse, FrontendRuntime::Docker));
        assert!(macos.supports(FsType::Nfs, FrontendRuntime::Host));
        assert!(!macos.supports(FsType::Fuse, FrontendRuntime::Host));
        assert_eq!(
            macos.default_runtime(FsType::Fuse),
            Some(FrontendRuntime::Libkrun)
        );

        let intel_macos = Platform {
            os: "macos",
            arch: "x86_64",
        };
        assert!(!intel_macos.supports(FsType::Fuse, FrontendRuntime::Libkrun));
        assert!(intel_macos.supports(FsType::Fuse, FrontendRuntime::Docker));
        assert_eq!(intel_macos.default_runtime(FsType::Fuse), None);

        let linux = Platform {
            os: "linux",
            arch: "x86_64",
        };
        assert!(linux.supports(FsType::Fuse, FrontendRuntime::Host));
        assert!(linux.supports(FsType::Fuse, FrontendRuntime::Docker));
        assert!(linux.supports(FsType::Nfs, FrontendRuntime::Host));
        assert_eq!(
            linux.default_runtime(FsType::Nfs),
            Some(FrontendRuntime::Host)
        );

        assert_eq!(
            default_runtime(FsType::Nfs),
            if cfg!(any(target_os = "macos", target_os = "linux")) {
                Some(FrontendRuntime::Host)
            } else {
                None
            }
        );
        assert_eq!(
            default_runtime(FsType::Fuse),
            if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
                Some(FrontendRuntime::Libkrun)
            } else if cfg!(target_os = "linux") {
                Some(FrontendRuntime::Host)
            } else {
                None
            }
        );
    }
}
