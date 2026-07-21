use super::spec_creation::CreatedMountSpec;
use crate::auth::AuthSelection;
use omnifs_workspace::authn::AuthKind;
use omnifs_workspace::ids::ProviderRef;
use omnifs_workspace::mounts::Name as MountName;
use omnifs_workspace::mounts::Spec;
use omnifs_workspace::mounts::{Auth, OAuth, StaticToken};

/// Compose the [`Spec`] for a newly authored mount.
pub(crate) fn mount_spec(
    mount_name: &MountName,
    reference: &ProviderRef,
    auth: Option<&AuthSelection>,
    scopes: &[String],
    created: CreatedMountSpec,
) -> Spec {
    Spec {
        provider: reference.clone(),
        mount: mount_name.to_string(),
        auth: auth.map(|auth| {
            let account = auth.account.clone();
            let scheme = auth.scheme.clone();
            match auth.auth_type {
                AuthKind::StaticToken => Auth::StaticToken(StaticToken { scheme, account }),
                AuthKind::OAuth => Auth::OAuth(OAuth {
                    scheme,
                    account,
                    scopes: (!scopes.is_empty()).then(|| scopes.to_vec()),
                    ..OAuth::default()
                }),
            }
        }),
        limits: created.limits,
        config_raw: created.config,
    }
}
