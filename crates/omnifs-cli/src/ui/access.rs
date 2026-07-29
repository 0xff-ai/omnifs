//! Typed recovery and access actions derived from Inventory.

use std::path::Path;

use omnifs_core::fs;

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

/// The one next-step hint both `omnifs setup`'s closing sentence and `mount
/// add`'s adaptive outro derive from the same three facts: a `Browse` hint
/// when `mount_name` and an attached host filesystem are both known, an
/// `AttachFilesystem` hint naming the platform's recommended filesystem when
/// a mount name is known but none is attached, or `None` when neither
/// applies (nothing was just mounted, or nothing is attached and this
/// platform recommends no filesystem at all).
pub(crate) fn mount_next_action_line(
    mount_name: Option<&str>,
    attached_host_location: Option<&Path>,
    recommended_fs_id: Option<&fs::Id>,
) -> Option<String> {
    let mount_name = mount_name?;
    if let Some(location) = attached_host_location {
        return Some(
            ActionLine::from(&NextAction::Browse {
                path: location.join(mount_name),
            })
            .render(),
        );
    }
    recommended_fs_id
        .map(|id| ActionLine::from(&NextAction::AttachFilesystem { id: id.clone() }).render())
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
    fn mount_next_action_line_browses_when_both_facts_are_known() {
        let location = PathBuf::from("/Users/raulk/omnifs");
        assert_eq!(
            mount_next_action_line(Some("dns"), Some(&location), None),
            Some("Browse:  `ls /Users/raulk/omnifs/dns`".to_owned())
        );
    }

    #[test]
    fn mount_next_action_line_offers_the_recommended_filesystem_when_none_is_attached() {
        let id: fs::Id = "nfs-host".parse().unwrap();
        assert_eq!(
            mount_next_action_line(Some("dns"), None, Some(&id)),
            Some("Mount files:  `omnifs fs attach --name nfs-host`".to_owned())
        );
    }

    #[test]
    fn mount_next_action_line_is_none_without_a_mount_name() {
        let location = PathBuf::from("/Users/raulk/omnifs");
        assert_eq!(mount_next_action_line(None, Some(&location), None), None);
    }

    #[test]
    fn mount_next_action_line_is_none_without_an_attach_or_a_recommendation() {
        assert_eq!(mount_next_action_line(Some("dns"), None, None), None);
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
