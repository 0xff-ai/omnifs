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
            NextAction::WaitForAttachment { id: _ } => Self {
                label: "Follow",
                command: "omnifs status --follow".to_owned(),
            },
            NextAction::CreateAttachment => Self {
                label: "Create an Attachment",
                command: "omnifs attachment add".to_owned(),
            },
            NextAction::Browse { path } => Self {
                label: "Browse",
                command: format!("ls {}", path.display()),
            },
            NextAction::EnterAttachment { id } => Self {
                label: "Enter",
                command: format!("omnifs attachment shell {id}"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::{
        AttachmentAccessState, AttachmentAccessStatus, AuthState, ServingState,
    };
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

    fn attachment(
        runtime: AttachmentRuntime,
        location: &str,
        state: AttachmentAccessState,
    ) -> AttachmentAccessStatus {
        let protocol = if runtime == AttachmentRuntime::Host && cfg!(target_os = "macos") {
            AttachmentProtocol::Nfs
        } else {
            AttachmentProtocol::Fuse
        };
        AttachmentAccessStatus {
            name: ResourceName::new(format!("attachment-{runtime}")).unwrap(),
            spec: AttachmentSpec::new(protocol, runtime, PathBuf::from(location), None, None)
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
                attachment(
                    AttachmentRuntime::Libkrun,
                    "/omnifs",
                    AttachmentAccessState::Attached,
                ),
                attachment(
                    AttachmentRuntime::Host,
                    "/mnt/omnifs-test-home/omnifs",
                    AttachmentAccessState::Attached,
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
            ActionLine::from(&NextAction::CreateAttachment).render(),
            "Create an Attachment:  `omnifs attachment add`"
        );
        assert_eq!(
            ActionLine::from(&NextAction::EnterAttachment {
                id: "guest".parse().unwrap()
            })
            .render(),
            "Enter:  `omnifs attachment shell guest`"
        );
    }

    #[test]
    fn a_failed_attachment_is_not_treated_as_observed_access() {
        let inventory = Inventory::test(
            DaemonHealth::Running,
            vec![attachment(
                AttachmentRuntime::Host,
                "/mnt",
                AttachmentAccessState::Failed,
            )],
            vec![mount("github")],
        );
        assert!(inventory.primary_host_location().is_none());
    }
}
