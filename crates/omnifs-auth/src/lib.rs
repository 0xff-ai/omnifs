mod callback;
mod client;
mod error;
mod flows;
mod request;
mod service;

pub use client::{OAuthClient, RevokeOutcome, UrlOpener};
pub use error::AuthError;
pub use flows::{DeviceCodePrompt, ManualCode};
pub use request::{
    DeviceCodeLoginRequest, LoginRequest, LoopbackLoginRequest, ManualCodeLoginRequest,
    OAuthRequest, OAuthRequestConfig,
};
pub use service::{
    AuthUnavailable, CredentialHealth, CredentialService, CredentialStatus, HeaderMaterial,
    REFRESH_WINDOW, is_fresh,
};

#[cfg(test)]
mod test_support;

#[cfg(test)]
mod tests;
