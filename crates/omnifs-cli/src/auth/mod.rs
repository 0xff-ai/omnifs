//! Shared auth helpers: the OAuth login flow, manifest views, mount auth
//! loading, and readiness probes. `omnifs mount add` drives credential acquisition;
//! there is no `omnifs auth` command surface.

pub(crate) mod explain;
pub(crate) mod login;
pub(crate) mod manifest_view;
pub(crate) mod mount;
pub(crate) mod readiness;

pub(crate) use login::login_with_workspace;
pub(crate) use manifest_view::AuthManifestView;
pub(crate) use mount::{AuthSelection, MountAuth};
pub(crate) use readiness::AuthReadiness;
