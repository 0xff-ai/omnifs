//! Typed recovery and access actions derived from Inventory.

use crate::inventory::NextAction;

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

impl From<&NextAction> for ActionLine {
    fn from(action: &NextAction) -> Self {
        match action {
            NextAction::Doctor { .. } => Self {
                label: "Fix",
                command: "omnifs doctor".to_owned(),
            },
            NextAction::Reauthenticate { mount } => Self {
                label: "Sign in",
                command: format!("omnifs mount reauth {mount}"),
            },
            NextAction::AttachFilesystem { id } => Self {
                label: "Mount files",
                command: format!("omnifs fs attach --name {id}"),
            },
            NextAction::CreateFilesystem => Self {
                label: "Create a filesystem",
                command: "omnifs fs create --name main".to_owned(),
            },
            NextAction::Browse { path } => Self {
                label: "Browse",
                command: format!("ls {}", path.display()),
            },
            NextAction::EnterFilesystem { id } => Self {
                label: "Enter",
                command: format!("omnifs fs shell --name {id}"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::{AuthState, FilesystemState, FilesystemStatus, ServingState};
    use crate::inventory::{DaemonHealth, Inventory, MountStatus, ProviderPin, ProviderPinState};
    use omnifs_core::fs;
    use omnifs_core::fs::Runtime;
    use std::path::{Path, PathBuf};

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
            inventory.primary_host_location(),
            Some(Path::new("/mnt/omnifs-test-home/omnifs"))
        );
    }

    #[test]
    fn typed_actions_render_one_pasteable_command() {
        assert_eq!(
            ActionLine::from(&NextAction::CreateFilesystem).render(),
            "Create a filesystem:  `omnifs fs create --name main`"
        );
        assert_eq!(
            ActionLine::from(&NextAction::EnterFilesystem {
                id: "guest".parse().unwrap()
            })
            .render(),
            "Enter:  `omnifs fs shell --name guest`"
        );
    }

    #[test]
    fn a_failed_filesystem_is_not_treated_as_observed_access() {
        let inventory = Inventory::test(
            DaemonHealth::Running,
            vec![filesystem(Runtime::Host, "/mnt", FilesystemState::Failed)],
            vec![mount("github")],
        );
        assert!(inventory.primary_host_location().is_none());
    }
}
