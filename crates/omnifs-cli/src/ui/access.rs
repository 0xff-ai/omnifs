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
use crate::inventory::{FrontendStatus, Inventory};

fn attached_frontends(inventory: &Inventory) -> Vec<&FrontendStatus> {
    inventory
        .frontends
        .iter()
        .filter(|frontend| frontend.state.provides_access())
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

fn browse_from_location(location: &Path, mount: Option<&str>) -> String {
    let target = mount.map_or_else(|| location.to_path_buf(), |name| location.join(name));
    format!("ls {}", omnifs_workspace::display(&target))
}

/// The guest-shell-or-enable-nudge tail shared by every browse action that
/// found no attached host frontend to name a path against.
fn browse_or_guest_fallback(inventory: &Inventory) -> String {
    if let Some(guest) = attached_frontends(inventory)
        .into_iter()
        .find(|frontend| frontend.runtime != Runtime::Host)
    {
        return guest_shell_command(guest);
    }
    "omnifs frontend enable nfs".to_owned()
}

/// Whether `omnifs status`'s closing `Browse:` line should print at all
/// (spec 3.1): suppressed whenever the context strip already carries a
/// `fix:` action naming the next step ([`DaemonState::context_fix`]), so the
/// human register never states two competing "what to do next" facts in the
/// same report. Bare `omnifs` (`cli.rs::run_bare`) already branches on
/// `DaemonState::Running` itself (spec 3.1's `Start serving:  omnifs up`
/// sentence) and does not use this.
pub(crate) fn show_browse_line(daemon_state: crate::inventory::DaemonState) -> bool {
    daemon_state.context_fix().is_none()
}

/// The single derived browse action for `omnifs status`'s closing
/// `Browse:` line: a host `ls` example when a host frontend is attached,
/// else the guest shell command, else the enable nudge. Never a bare path
/// claim when nothing is observed. Names whichever mount sorts first, since
/// no single mount is more relevant than another to a whole-workspace
/// summary.
pub(crate) fn browse_command(inventory: &Inventory) -> String {
    match primary_host_location(inventory) {
        Some(location) => {
            browse_from_location(location, inventory.mounts.first().map(|m| m.name.as_str()))
        },
        None => browse_or_guest_fallback(inventory),
    }
}

/// One compact access fact for `mount show`'s detail card (spec 3.4):
/// `<path>  (<filesystem> <runtime>)`, reusing the same filesystem/runtime
/// vocabulary as [`lines`]'s full sentences. Callers filter
/// [`crate::inventory::AccessPath`]s to the ones worth showing (a card has no
/// use for a `Failed` row's dead path) before mapping through this.
pub(crate) fn access_row(path: &crate::inventory::AccessPath) -> String {
    format!(
        "{}  ({} {})",
        omnifs_workspace::display(&path.path),
        path.filesystem.label(),
        path.runtime.label()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::frontend::FrontendFilesystem as Filesystem;
    use crate::inventory::{AuthState, FrontendState, ServingState};
    use crate::inventory::{DaemonState, MountStatus, ProviderPin, ProviderPinState};
    use omnifs_workspace::mounts::Name as MountName;
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
    fn browse_line_is_suppressed_exactly_when_the_context_strip_already_has_a_fix_action() {
        use crate::inventory::DaemonState;

        // Stopped/Failed/Unreachable all print a `fix:` action in
        // `status.rs::render`'s context strip, so the closing `Browse:` line
        // would restate a competing "what to do next" fact.
        for suppressed in [
            DaemonState::Stopped,
            DaemonState::Failed,
            DaemonState::Unreachable,
        ] {
            assert!(!show_browse_line(suppressed), "{suppressed:?}");
        }
        for shown in [
            DaemonState::Running,
            DaemonState::Starting,
            DaemonState::Degraded,
        ] {
            assert!(show_browse_line(shown), "{shown:?}");
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
    fn browse_command_defers_to_the_first_mount() {
        let inventory = Inventory::test(
            DaemonState::Running,
            vec![frontend(
                Runtime::Host,
                Some("/mnt/omnifs-test-home/omnifs"),
                FrontendState::Attached,
            )],
            vec![mount("aaa-sorts-first"), mount("github")],
        );
        assert_eq!(
            browse_command(&inventory),
            "ls /mnt/omnifs-test-home/omnifs/aaa-sorts-first"
        );
    }

    #[test]
    fn access_row_names_path_filesystem_and_runtime() {
        let inventory = Inventory::test(
            DaemonState::Running,
            vec![frontend(
                Runtime::Host,
                Some("/mnt/omnifs-test-home/omnifs"),
                FrontendState::Attached,
            )],
            vec![mount("github")],
        );
        let paths = inventory.access_paths(&MountName::new("github").unwrap());
        let rows: Vec<String> = paths.iter().map(access_row).collect();
        assert_eq!(
            rows,
            vec!["/mnt/omnifs-test-home/omnifs/github  (fuse host)".to_owned()]
        );
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
