use super::spec_creation::CreatedMountSpec;
use crate::auth::AuthSelection;
use omnifs_workspace::authn::AuthKind;
use omnifs_workspace::ids::ProviderRef;
use omnifs_workspace::mounts::Name as MountName;
use omnifs_workspace::mounts::Spec;
use omnifs_workspace::mounts::{Auth, OAuth, StaticToken};

/// Composes the [`Spec`] for a newly authored mount. The on-disk JSON is the
/// `Spec`'s own serialization, persisted atomically by `Registry::put`; this
/// type only assembles the in-memory value.
pub(super) struct MountFile<'a> {
    mount_name: &'a MountName,
    /// The pinned provider reference written into the mount spec, taken from the
    /// latest installed artifact for this provider.
    reference: &'a ProviderRef,
    auth: Option<&'a AuthSelection>,
    scopes: &'a [String],
    created: CreatedMountSpec,
}

impl<'a> MountFile<'a> {
    pub(super) fn new(
        mount_name: &'a MountName,
        reference: &'a ProviderRef,
        auth: Option<&'a AuthSelection>,
        scopes: &'a [String],
        created: CreatedMountSpec,
    ) -> Self {
        Self {
            mount_name,
            reference,
            auth,
            scopes,
            created,
        }
    }

    pub(super) fn into_spec(self) -> Spec {
        Spec {
            provider: self.reference.clone(),
            mount: self.mount_name.to_string(),
            root_mount: false,
            revalidate: true,
            auth: self.auth.map(|auth| {
                let account = auth.account.clone();
                let scheme = auth.scheme.clone();
                match auth.auth_type {
                    AuthKind::StaticToken => Auth::StaticToken(StaticToken { scheme, account }),
                    AuthKind::OAuth => Auth::OAuth(OAuth {
                        scheme,
                        account,
                        scopes: (!self.scopes.is_empty()).then(|| self.scopes.to_vec()),
                        ..OAuth::default()
                    }),
                }
            }),
            capabilities: self.created.capabilities,
            config_raw: self.created.config,
        }
    }
}
