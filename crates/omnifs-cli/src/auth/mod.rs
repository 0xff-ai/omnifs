//! Shared auth helpers: the OAuth login flow, manifest views, mount auth
//! loading, and readiness probes. `omnifs init` drives credential acquisition;
//! there is no `omnifs auth` command surface.

pub(crate) mod explain;
pub(crate) mod login;
pub(crate) mod manifest_view;
pub(crate) mod mount;
pub(crate) mod readiness;

pub(crate) use login::login_with_workspace;
pub(crate) use manifest_view::AuthManifestView;
pub(crate) use mount::{AuthSelection, MountAuth, load_mount_auth, mount_auth};
pub(crate) use readiness::{AuthProbeSeverity, AuthProbeSummary, AuthReadiness, AuthTerminalKind};
