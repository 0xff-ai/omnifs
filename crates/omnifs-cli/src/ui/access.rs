//! The access-line owner (spec 2.9): "where are my files." `up`, `status`,
//! and bare `omnifs` all speak through this module rather than each
//! reimplementing the join.
//!
//! There is no persisted frontend desired state by design, so this module
//! never invents a claim: every line here is derived from
//! [`Inventory::frontends`], the same join of the daemon's live attachments
//! with workspace runner observations that `omnifs status` renders as the
//! Frontends table. An attached host frontend names its location; an
//! attached guest frontend (Docker, libkrun) names the shell command instead,
//! since its wire mount point is display-only and not host-reachable; no
//! observed frontend at all names the enable command instead of claiming any
//! path.

use std::path::Path;

use crate::commands::frontend::FrontendRuntime as Runtime;
use crate::inventory::{FrontendState, FrontendStatus, Inventory};

fn attached_frontends(inventory: &Inventory) -> Vec<&FrontendStatus> {
    inventory
        .frontends
        .iter()
        .filter(|frontend| {
            matches!(
                frontend.state,
                FrontendState::Attached | FrontendState::Running
            )
        })
        .collect()
}

/// The first attached host frontend's location, if any. `up`'s no-op
/// one-liner and every other "Files at" surface name only this primary
/// surface; a full access block (see [`lines`]) still lists every attached
/// host location.
pub(crate) fn primary_host_location(inventory: &Inventory) -> Option<&Path> {
    attached_frontends(inventory)
        .into_iter()
        .find(|frontend| frontend.runtime == Runtime::Host)
        .and_then(|frontend| frontend.location.as_deref())
}

fn host_location_line(frontend: &FrontendStatus) -> String {
    let location = frontend
        .location
        .as_deref()
        .map_or_else(|| "~/omnifs".to_owned(), omnifs_workspace::display);
    format!("Files at {location}  ({})", frontend.filesystem.label())
}

fn guest_shell_command(frontend: &FrontendStatus) -> String {
    format!(
        "omnifs frontend shell {} --runtime {}",
        frontend.filesystem.label(),
        frontend.runtime.label()
    )
}

fn guest_shell_line(frontend: &FrontendStatus) -> String {
    format!("In the microVM:  `{}`", guest_shell_command(frontend))
}

fn no_frontend_line(mount_count: usize) -> String {
    let noun = if mount_count == 1 { "mount" } else { "mounts" };
    format!("Serving {mount_count} {noun}. No frontend attached yet:  `omnifs frontend enable nfs`")
}

/// The full access block for a surface's closing lines (`up`, bare
/// `omnifs`): one line per attached host location, one per attached guest
/// runtime, or the single "no frontend attached yet" nudge when nothing is
/// observed at all. Commands are backtick-marked, per the crate-wide
/// convention that the caller's narration (`Output::narrate`) turns
/// backtick spans into the accent color and drops the backticks (spec 2.4),
/// so this module never has to probe or receive real terminal capabilities.
pub(crate) fn lines(inventory: &Inventory) -> Vec<String> {
    let attached = attached_frontends(inventory);
    if attached.is_empty() {
        return vec![no_frontend_line(inventory.mounts.len())];
    }
    attached
        .into_iter()
        .map(|frontend| match frontend.runtime {
            Runtime::Host => host_location_line(frontend),
            Runtime::Docker | Runtime::Libkrun => guest_shell_line(frontend),
        })
        .collect()
}

/// The single derived browse action for `omnifs status`'s closing
/// `Browse:` line: a host `ls` example when a host frontend is attached,
/// else the guest shell command, else the enable nudge. Never a bare path
/// claim when nothing is observed.
pub(crate) fn browse_command(inventory: &Inventory) -> String {
    if let Some(location) = primary_host_location(inventory) {
        return match inventory.mounts.first() {
            Some(mount) => format!(
                "ls {}",
                omnifs_workspace::display(&location.join(&mount.name))
            ),
            None => format!("ls {}", omnifs_workspace::display(location)),
        };
    }
    if let Some(guest) = attached_frontends(inventory)
        .into_iter()
        .find(|frontend| frontend.runtime != Runtime::Host)
    {
        return guest_shell_command(guest);
    }
    "omnifs frontend enable nfs".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::frontend::FrontendFilesystem as Filesystem;
    use crate::inventory::{AuthState, ServingState};
    use crate::inventory::{DaemonState, MountStatus, ProviderPin, ProviderPinState};
    use std::path::PathBuf;

    fn mount(name: &str) -> MountStatus {
        MountStatus {
            name: name.to_owned(),
            root: PathBuf::from(format!("/{name}")),
            provider: ProviderPin {
                name: name.to_owned(),
                version: None,
                artifact: "a".repeat(64),
                state: ProviderPinState::Available,
            },
            auth: AuthState::NotNeeded,
            serving: ServingState::Live,
            access_count: 1,
            fix: None,
        }
    }

    fn frontend(runtime: Runtime, location: Option<&str>, state: FrontendState) -> FrontendStatus {
        FrontendStatus {
            filesystem: Filesystem::Fuse,
            runtime,
            location: location.map(PathBuf::from),
            state,
            scope: "all",
            mount_count: 1,
            fix: None,
        }
    }

    #[test]
    fn no_observed_frontend_names_the_enable_command_not_a_path() {
        let inventory = Inventory::test(DaemonState::Running, Vec::new(), vec![mount("github")]);
        let rendered = lines(&inventory);
        assert_eq!(rendered.len(), 1);
        assert!(rendered[0].starts_with("Serving 1 mount. No frontend attached yet:"));
        assert!(rendered[0].contains("omnifs frontend enable nfs"));
        assert_eq!(browse_command(&inventory), "omnifs frontend enable nfs");
    }

    #[test]
    fn attached_host_frontend_names_its_location() {
        let inventory = Inventory::test(
            DaemonState::Running,
            vec![frontend(
                Runtime::Host,
                Some("/mnt/omnifs-test-home/omnifs"),
                FrontendState::Attached,
            )],
            vec![mount("github")],
        );
        let rendered = lines(&inventory);
        assert_eq!(
            rendered,
            vec!["Files at /mnt/omnifs-test-home/omnifs  (fuse)"]
        );
        assert_eq!(
            primary_host_location(&inventory),
            Some(Path::new("/mnt/omnifs-test-home/omnifs"))
        );
        assert_eq!(
            browse_command(&inventory),
            "ls /mnt/omnifs-test-home/omnifs/github"
        );
    }

    #[test]
    fn attached_guest_frontend_names_the_shell_command_not_the_wire_mount_point() {
        let inventory = Inventory::test(
            DaemonState::Running,
            vec![frontend(
                Runtime::Libkrun,
                Some("/omnifs"),
                FrontendState::Attached,
            )],
            vec![mount("github")],
        );
        let rendered = lines(&inventory);
        assert_eq!(rendered.len(), 1);
        assert!(rendered[0].starts_with("In the microVM:"));
        assert!(rendered[0].contains("omnifs frontend shell fuse --runtime libkrun"));
        assert_eq!(
            browse_command(&inventory),
            "omnifs frontend shell fuse --runtime libkrun"
        );
    }

    #[test]
    fn host_takes_precedence_over_guest_for_the_primary_browse_action() {
        let inventory = Inventory::test(
            DaemonState::Running,
            vec![
                frontend(Runtime::Libkrun, Some("/omnifs"), FrontendState::Attached),
                frontend(
                    Runtime::Host,
                    Some("/mnt/omnifs-test-home/omnifs"),
                    FrontendState::Attached,
                ),
            ],
            vec![mount("github")],
        );
        assert_eq!(
            browse_command(&inventory),
            "ls /mnt/omnifs-test-home/omnifs/github"
        );
        assert_eq!(lines(&inventory).len(), 2);
    }

    #[test]
    fn a_failed_frontend_is_not_treated_as_observed_access() {
        let inventory = Inventory::test(
            DaemonState::Running,
            vec![frontend(Runtime::Host, Some("/mnt"), FrontendState::Failed)],
            vec![mount("github")],
        );
        assert!(primary_host_location(&inventory).is_none());
        assert_eq!(browse_command(&inventory), "omnifs frontend enable nfs");
    }
}
