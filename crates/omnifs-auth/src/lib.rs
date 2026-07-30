mod callback;
mod client;
mod credentials;
mod error;
mod flows;
mod ids;
mod request;
mod resolve;
mod scheme;
mod service;

pub use client::{OAuthClient, OAuthRevokeOutcome, UrlOpener};
pub use credentials::CredentialEntry;
pub use error::AuthError;
pub use flows::{DeviceCodePrompt, ManualCode};
pub use ids::{AccountId, AuthKind, CredentialId, CredentialIdError, SchemeId};
pub use request::{
    DeviceCodeLoginRequest, LoginRequest, LoopbackLoginRequest, ManualCodeLoginRequest,
    OAuthRequest, OAuthRequestConfig, OAuthRuntimeOverrides,
};
pub use resolve::SchemeResolveError;
pub use scheme::{
    AmbientKind, AmbientSource, AuthManifest, AuthScheme, ClientSideTokenConfig, DeviceCodeConfig,
    DevicePollCompat, KeyValue, OAuthFlow, OauthScheme, PkceLoopbackConfig, PkceManualCodeConfig,
    SchemeGuidance, StaticTokenScheme, TokenEndpointAuthMethod, TokenValidation,
};
pub use service::{
    AuthBinding, AuthUnavailable, CredentialHealth, CredentialService, DurableCredentialSnapshot,
    REFRESH_WINDOW, RefreshCandidate, RefreshClassification, RefreshOutcome, RefreshPersistError,
    RefreshPersistence, RefreshSink, RejectionEvidence,
};

#[cfg(test)]
mod test_support;

#[cfg(test)]
mod tests;
