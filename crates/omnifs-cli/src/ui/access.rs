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
            NextAction::WaitForFilesystem { id: _ } => Self {
                label: "Follow",
                command: "omnifs status --follow".to_owned(),
            },
            NextAction::CreateFilesystem => Self {
                label: "Create an Filesystem",
                command: "omnifs fs add".to_owned(),
            },
            NextAction::Browse { path } => Self {
                label: "Browse",
                command: format!("ls {}", path.display()),
            },
            NextAction::EnterFilesystem { id } => Self {
                label: "Enter",
                command: format!("omnifs fs shell {id}"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::{
        AuthState, FilesystemAccessState, FilesystemAccessStatus, ServingState,
    };
    use crate::inventory::{DaemonHealth, Inventory, MountStatus, ProviderPin, ProviderPinState};
    use omnifs_core::{FilesystemProtocol, FilesystemRuntime, FilesystemSpec, ResourceName};
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
        runtime: FilesystemRuntime,
        location: &str,
        state: FilesystemAccessState,
    ) -> FilesystemAccessStatus {
        let protocol = if runtime == FilesystemRuntime::Host && cfg!(target_os = "macos") {
            FilesystemProtocol::Nfs
        } else {
            FilesystemProtocol::Fuse
        };
        FilesystemAccessStatus {
            name: ResourceName::new(format!("filesystem-{runtime}")).unwrap(),
            spec: FilesystemSpec::new(protocol, runtime, PathBuf::from(location), None, None)
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
                    FilesystemRuntime::Libkrun,
                    "/omnifs",
                    FilesystemAccessState::Ready,
                ),
                filesystem(
                    FilesystemRuntime::Host,
                    "/mnt/omnifs-test-home/omnifs",
                    FilesystemAccessState::Ready,
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
            "Create an Filesystem:  `omnifs fs add`"
        );
        assert_eq!(
            ActionLine::from(&NextAction::EnterFilesystem {
                id: "guest".parse().unwrap()
            })
            .render(),
            "Enter:  `omnifs fs shell guest`"
        );
    }

    #[test]
    fn a_failed_filesystem_is_not_treated_as_observed_access() {
        let inventory = Inventory::test(
            DaemonHealth::Running,
            vec![filesystem(
                FilesystemRuntime::Host,
                "/mnt",
                FilesystemAccessState::Failed,
            )],
            vec![mount("github")],
        );
        assert!(inventory.primary_host_location().is_none());
    }
}
