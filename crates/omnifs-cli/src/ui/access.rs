//! The access-line owner: "where are my files." `up`, `status`,
//! and bare `omnifs` all speak through this module rather than each
//! reimplementing the join.
//!
//! Configured filesystem identity comes from persisted specs while attachment
//! state comes from the daemon. An attached host filesystem names its location; an
//! attached guest filesystem (Docker, libkrun) names the shell command instead,
//! since its wire mount point is display-only and not host-reachable; no
//! observed filesystem at all names the create command instead of claiming any
//! path.

use omnifs_core::fs::Runtime;
use std::path::Path;

use crate::inventory::{FilesystemStatus, Inventory};

fn attached_filesystems(inventory: &Inventory) -> Vec<&FilesystemStatus> {
    inventory
        .filesystems
        .iter()
        .filter(|filesystem| filesystem.state.provides_access())
        .collect()
}

/// The first attached host filesystem's location, if any. `up`'s no-op
/// one-liner and every other "Files at" surface name only this primary
/// surface; a full access block (see [`lines`]) still lists every attached
/// host location.
pub(crate) fn primary_host_location(inventory: &Inventory) -> Option<&Path> {
    attached_filesystems(inventory)
        .into_iter()
        .find(|filesystem| filesystem.spec.runtime() == Runtime::Host)
        .map(|filesystem| filesystem.spec.location())
}

fn host_location_line(filesystem: &FilesystemStatus) -> String {
    let location = omnifs_workspace::display(filesystem.spec.location());
    format!(
        "Files at {location}  ({})",
        filesystem.spec.protocol().as_str()
    )
}

fn guest_shell_command(filesystem: &FilesystemStatus) -> String {
    format!("omnifs fs shell --name {}", filesystem.spec.id())
}

pub(crate) fn guest_shell_line(filesystem: &FilesystemStatus) -> String {
    format!("In the microVM:  `{}`", guest_shell_command(filesystem))
}

fn no_filesystem_line(mount_count: usize) -> String {
    let noun = if mount_count == 1 { "mount" } else { "mounts" };
    format!(
        "Serving {mount_count} {noun}. No filesystem attached yet:  `omnifs fs create --name main`"
    )
}

/// The full access block for a surface's closing lines (`up`, bare
/// `omnifs`): one line per attached host location, one per attached guest
/// runtime, or the single "no filesystem attached yet" nudge when nothing is
/// observed at all. Commands are backtick-marked, per the crate-wide
/// convention that the caller's narration (`Output::narrate`) turns
/// backtick spans into the accent color and drops the backticks,
/// so this module never has to probe or receive real terminal capabilities.
pub(crate) fn lines(inventory: &Inventory) -> Vec<String> {
    let attached = attached_filesystems(inventory);
    if attached.is_empty() {
        return vec![no_filesystem_line(inventory.mounts.len())];
    }
    attached
        .into_iter()
        .map(|filesystem| match filesystem.spec.runtime() {
            Runtime::Host => host_location_line(filesystem),
            Runtime::Docker | Runtime::Libkrun => guest_shell_line(filesystem),
        })
        .collect()
}

fn browse_from_location(location: &Path, mount: Option<&str>) -> String {
    let target = mount.map_or_else(|| location.to_path_buf(), |name| location.join(name));
    format!("ls {}", omnifs_workspace::display(&target))
}

/// The guest-shell-or-create-nudge tail shared by every browse action that
/// found no attached host filesystem to name a path against.
fn browse_or_guest_fallback(inventory: &Inventory) -> String {
    if let Some(guest) = attached_filesystems(inventory)
        .into_iter()
        .find(|filesystem| filesystem.spec.runtime() != Runtime::Host)
    {
        return guest_shell_command(guest);
    }
    "omnifs fs create --name main".to_owned()
}

/// Whether `omnifs status`'s closing `Browse:` line should print at all
/// suppressed whenever the context strip already carries a
/// `fix:` action naming the next step ([`DaemonHealth::context_fix`]), so the
/// human register never states two competing "what to do next" facts in the
/// same report. Bare `omnifs` (`cli.rs::run_bare`) already branches on
/// `DaemonHealth::Running` itself and does not use this.
pub(crate) fn show_browse_line(daemon_health: crate::inventory::DaemonHealth) -> bool {
    daemon_health.context_fix().is_none()
}

/// The single derived browse action for `omnifs status`'s closing
/// `Browse:` line: a host `ls` example when a host filesystem is attached,
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

/// One compact access fact for `mount show`'s detail card:
/// `<path>  (<filesystem> <runtime>)`, reusing the same filesystem/runtime
/// vocabulary as [`lines`]'s full sentences. Callers filter
/// [`crate::inventory::AccessPath`]s to the ones worth showing (a card has no
/// use for a `Failed` row's dead path) before mapping through this.
pub(crate) fn access_row(path: &crate::inventory::AccessPath) -> String {
    format!(
        "{}  ({} {})",
        omnifs_workspace::display(&path.path),
        path.protocol.as_str(),
        path.runtime.as_str()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::{AuthState, FilesystemState, ServingState};
    use crate::inventory::{DaemonHealth, MountStatus, ProviderPin, ProviderPinState};
    use omnifs_core::MountName;
    use omnifs_core::fs;
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

    fn filesystem(runtime: Runtime, location: &str, state: FilesystemState) -> FilesystemStatus {
        FilesystemStatus {
            spec: fs::Spec::new(
                format!("fuse-{runtime}").parse().unwrap(),
                fs::Protocol::Fuse,
                runtime,
                PathBuf::from(location),
            )
            .unwrap(),
            state,
            mount_count: 1,
            fix: None,
        }
    }

    #[test]
    fn browse_line_is_suppressed_exactly_when_the_context_strip_already_has_a_fix_action() {
        use crate::inventory::DaemonHealth;

        // Stopped/Failed/Unreachable all print a `fix:` action in
        // `status.rs::render`'s context strip, so the closing `Browse:` line
        // would restate a competing "what to do next" fact.
        for suppressed in [
            DaemonHealth::Stopped,
            DaemonHealth::Failed,
            DaemonHealth::Unreachable,
        ] {
            assert!(!show_browse_line(suppressed), "{suppressed:?}");
        }
        for shown in [
            DaemonHealth::Running,
            DaemonHealth::Starting,
            DaemonHealth::Degraded,
        ] {
            assert!(show_browse_line(shown), "{shown:?}");
        }
    }

    #[test]
    fn no_observed_filesystem_names_the_create_command_not_a_path() {
        let inventory = Inventory::test(DaemonHealth::Running, Vec::new(), vec![mount("github")]);
        let rendered = lines(&inventory);
        assert_eq!(rendered.len(), 1);
        assert!(rendered[0].starts_with("Serving 1 mount. No filesystem attached yet:"));
        assert!(rendered[0].contains("omnifs fs create --name main"));
        assert_eq!(browse_command(&inventory), "omnifs fs create --name main");
    }

    #[test]
    fn attached_host_filesystem_names_its_location() {
        let inventory = Inventory::test(
            DaemonHealth::Running,
            vec![filesystem(
                Runtime::Host,
                "/mnt/omnifs-test-home/omnifs",
                FilesystemState::Attached,
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
    fn attached_guest_filesystem_names_the_shell_command_not_the_wire_mount_point() {
        let inventory = Inventory::test(
            DaemonHealth::Running,
            vec![filesystem(
                Runtime::Libkrun,
                "/omnifs",
                FilesystemState::Attached,
            )],
            vec![mount("github")],
        );
        let rendered = lines(&inventory);
        assert_eq!(rendered.len(), 1);
        assert!(rendered[0].starts_with("In the microVM:"));
        assert!(rendered[0].contains("omnifs fs shell --name fuse-libkrun"));
        assert_eq!(
            browse_command(&inventory),
            "omnifs fs shell --name fuse-libkrun"
        );
    }

    #[test]
    fn host_takes_precedence_over_guest_for_the_primary_browse_action() {
        let inventory = Inventory::test(
            DaemonHealth::Running,
            vec![
                filesystem(Runtime::Libkrun, "/omnifs", FilesystemState::Attached),
                filesystem(
                    Runtime::Host,
                    "/mnt/omnifs-test-home/omnifs",
                    FilesystemState::Attached,
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
            DaemonHealth::Running,
            vec![filesystem(
                Runtime::Host,
                "/mnt/omnifs-test-home/omnifs",
                FilesystemState::Attached,
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
            DaemonHealth::Running,
            vec![filesystem(
                Runtime::Host,
                "/mnt/omnifs-test-home/omnifs",
                FilesystemState::Attached,
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
    fn a_failed_filesystem_is_not_treated_as_observed_access() {
        let inventory = Inventory::test(
            DaemonHealth::Running,
            vec![filesystem(Runtime::Host, "/mnt", FilesystemState::Failed)],
            vec![mount("github")],
        );
        assert!(primary_host_location(&inventory).is_none());
        assert_eq!(browse_command(&inventory), "omnifs fs create --name main");
    }
}
