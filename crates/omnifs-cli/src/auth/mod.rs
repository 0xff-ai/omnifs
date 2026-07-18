//! Shared auth helpers: the OAuth login flow, manifest views, mount auth
//! loading, and readiness probes. `omnifs mount add` drives credential acquisition;
//! there is no `omnifs auth` command surface.

pub(crate) mod explain;
pub(crate) mod login;
pub(crate) mod manifest_view;
pub(crate) mod mount;
pub(crate) mod readiness;

pub(crate) use login::{LoginInteractivity, login_with_workspace};
pub(crate) use manifest_view::AuthManifestView;
pub(crate) use mount::{AuthSelection, MountAuth};
pub(crate) use readiness::AuthReadiness;

/// Keys any completed-auth receipt row may use:
/// `oauth` (device-code flow, [`login::login_with_spec`]/
/// [`login::login_with_workspace`]), `signed in` (every other completed auth
/// path), `credential` (static-token store and ambient import). Shared
/// verbatim by `mount add`'s and `mount reauth`'s auth blocks, since both
/// route through the same `login`/`run_static_token_init`/
/// `AuthImportDecision` primitives that print these rows; each caller still
/// owns its own wider block (`mount add` folds this into
/// `stages::mount_add_key_width`) and passes the resulting width down, since
/// these shared primitives cannot know which flow invoked them.
pub(crate) const AUTH_RECEIPT_KEYS: [&str; 3] = ["oauth", "signed in", "credential"];

pub(crate) fn auth_receipt_key_width() -> usize {
    crate::ui::render::key_field_width(&AUTH_RECEIPT_KEYS)
}
