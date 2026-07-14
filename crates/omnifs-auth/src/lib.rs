mod callback;
mod client;
mod error;
mod flows;
mod request;
mod service;

pub use client::{OAuthClient, OAuthRevokeOutcome, UrlOpener};
pub use error::AuthError;
pub use flows::{DeviceCodePrompt, ManualCode};
pub use request::{
    DeviceCodeLoginRequest, LoginRequest, LoopbackLoginRequest, ManualCodeLoginRequest,
    OAuthRequest,
};
pub use service::{
    AuthUnavailable, CredentialHealth, CredentialService, CredentialStatus, REFRESH_WINDOW,
    RefreshOutcome, RejectionEvidence, RevokeOutcome,
};

#[cfg(test)]
mod test_support;

#[cfg(test)]
mod tests;
