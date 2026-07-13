//! `omnifs frontend ls`: live attachments reported by the daemon.

use clap::Args;
use omnifs_api::FrontendInfo;

use crate::error::ExitCode;
use crate::ui::report::Report;
use crate::workspace::Workspace;

#[derive(Args, Debug, Clone, Default)]
pub struct FrontendLsArgs {}

impl FrontendLsArgs {
    pub async fn run(self) -> anyhow::Result<ExitCode> {
        let workspace = Workspace::resolve()?;
        let frontends = workspace
            .daemon()
            .compatible_status_optional()
            .await?
            .map_or_else(Vec::new, |status| status.frontends);
        frontend_report(&frontends).print();
        Ok(ExitCode::Success)
    }
}

fn frontend_report(frontends: &[FrontendInfo]) -> Report {
    let mut report = Report::new();
    report.push(crate::status::frontend_section(frontends));
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_api::{FrontendDelivery, FsType};
    use std::path::PathBuf;

    fn frontend(fs_type: FsType, delivery: FrontendDelivery, mount_point: &str) -> FrontendInfo {
        FrontendInfo {
            fs_type,
            delivery,
            mount_point: PathBuf::from(mount_point),
            source: "omnifs".to_string(),
        }
    }

    #[test]
    fn report_lists_every_live_attachment_and_marks_guest_paths() {
        let report = frontend_report(&[
            frontend(FsType::Nfs, FrontendDelivery::Local, "/Users/me/omnifs"),
            frontend(FsType::Fuse, FrontendDelivery::Docker, "/omnifs"),
        ]);
        let rendered = crate::ui::strip_ansi(&report.render());

        assert!(rendered.contains("Frontends (2)"));
        assert!(rendered.contains("nfs (local)"));
        assert!(rendered.contains("/Users/me/omnifs"));
        assert!(rendered.contains("fuse (docker)"));
        assert!(rendered.contains("/omnifs (guest)"));
    }

    #[test]
    fn report_is_explicit_when_no_frontend_is_attached() {
        let rendered = crate::ui::strip_ansi(&frontend_report(&[]).render());
        assert!(rendered.contains("Frontends (0)"));
        assert!(rendered.contains("no frontends attached"));
    }
}
