//! Typed recovery and access actions derived from Inventory.

use omnifs_core::fs::Runtime;
use std::path::Path;

use crate::inventory::{FilesystemStatus, Inventory, NextAction};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ActionLine {
    pub(crate) label: &'static str,
    pub(crate) command: String,
}

impl ActionLine {
    pub(crate) fn render(&self) -> String {
        format!("{}:  `{}`", self.label, self.command)
    }
}

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

pub(crate) fn action_line(action: &NextAction) -> ActionLine {
    match action {
        NextAction::Doctor { .. } => ActionLine {
            label: "Fix",
            command: "omnifs doctor".to_owned(),
        },
        NextAction::Reauthenticate { mount } => ActionLine {
            label: "Sign in",
            command: format!("omnifs mount reauth {mount}"),
        },
        NextAction::StartDaemon => ActionLine {
            label: "Start serving",
            command: "omnifs up".to_owned(),
        },
        NextAction::AttachFilesystem { id } => ActionLine {
            label: "Mount files",
            command: format!("omnifs fs attach --name {id}"),
        },
        NextAction::CreateFilesystem => ActionLine {
            label: "Create a filesystem",
            command: "omnifs fs create --name main".to_owned(),
        },
        NextAction::Browse { path } => ActionLine {
            label: "Browse",
            command: format!("ls {}", omnifs_workspace::display(path)),
        },
        NextAction::EnterFilesystem { id } => ActionLine {
            label: "Enter",
            command: format!("omnifs fs shell --name {id}"),
        },
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
    fn host_takes_precedence_for_the_primary_location() {
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
            primary_host_location(&inventory),
            Some(Path::new("/mnt/omnifs-test-home/omnifs"))
        );
    }

    #[test]
    fn typed_actions_render_one_pasteable_command() {
        assert_eq!(
            action_line(&NextAction::CreateFilesystem).render(),
            "Create a filesystem:  `omnifs fs create --name main`"
        );
        assert_eq!(
            action_line(&NextAction::EnterFilesystem {
                id: "guest".parse().unwrap()
            })
            .render(),
            "Enter:  `omnifs fs shell --name guest`"
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
    }
}
