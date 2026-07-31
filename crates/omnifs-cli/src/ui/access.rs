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
            NextAction::AttachFilesystem { id: _ } => Self {
                label: "Mount files",
                command: "omnifs attachment add".to_owned(),
            },
            NextAction::CreateFilesystem => Self {
                label: "Create a filesystem",
                command: "omnifs attachment add".to_owned(),
            },
            NextAction::Browse { path } => Self {
                label: "Browse",
                command: format!("ls {}", path.display()),
            },
            NextAction::EnterFilesystem { id } => Self {
                label: "Enter",
                command: format!("omnifs attachment shell {id}"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::{AuthState, FilesystemState, FilesystemStatus, ServingState};
    use crate::inventory::{DaemonHealth, Inventory, MountStatus, ProviderPin, ProviderPinState};
    use omnifs_core::{AttachmentProtocol, AttachmentRuntime, AttachmentSpec, ResourceName};
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

    fn filesystem(
        runtime: AttachmentRuntime,
        location: &str,
        state: FilesystemState,
    ) -> FilesystemStatus {
        let protocol = if runtime == AttachmentRuntime::Host && cfg!(target_os = "macos") {
            AttachmentProtocol::Nfs
        } else {
            AttachmentProtocol::Fuse
        };
        FilesystemStatus {
            name: ResourceName::new(format!("attachment-{runtime}")).unwrap(),
            spec: AttachmentSpec::new(
                protocol,
                runtime,
                PathBuf::from(location),
                None,
                None,
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
                filesystem(
                    AttachmentRuntime::Libkrun,
                    "/omnifs",
                    FilesystemState::Attached,
                ),
                filesystem(
                    AttachmentRuntime::Host,
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
            "Create a filesystem:  `omnifs attachment add`"
        );
        assert_eq!(
            ActionLine::from(&NextAction::EnterFilesystem {
                id: "guest".parse().unwrap()
            })
            .render(),
            "Enter:  `omnifs attachment shell guest`"
        );
    }

    #[test]
    fn a_failed_filesystem_is_not_treated_as_observed_access() {
        let inventory = Inventory::test(
            DaemonHealth::Running,
            vec![filesystem(
                AttachmentRuntime::Host,
                "/mnt",
                FilesystemState::Failed,
            )],
            vec![mount("github")],
        );
        assert!(inventory.primary_host_location().is_none());
    }
}
