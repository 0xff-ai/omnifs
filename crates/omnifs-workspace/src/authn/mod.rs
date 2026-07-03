//! The auth vocabulary: the provider-declared auth-scheme wire model and
//! scheme resolution.

pub mod ids;
pub mod resolve;
pub mod scheme;

pub use ids::{AccountId, AuthKind, CredentialId, CredentialIdError, SchemeId};
pub use resolve::SchemeResolveError;
pub use scheme::{
    AuthManifest, AuthScheme, ClientSideTokenConfig, DeviceCodeConfig, KeyValue, OAuthFlow,
    OauthScheme, PkceLoopbackConfig, PkceManualCodeConfig, SchemeGuidance, StaticTokenScheme,
    TokenEndpointAuthMethod, TokenValidation,
};
