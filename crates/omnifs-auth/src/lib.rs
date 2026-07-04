mod callback;
mod client;
mod error;
mod flows;
mod request;

pub use client::{OAuthClient, RevokeOutcome, UrlOpener};
pub use error::AuthError;
pub use flows::{DeviceCodePrompt, ManualCode};
pub use request::{
    DeviceCodeLoginRequest, LoginRequest, LoopbackLoginRequest, ManualCodeLoginRequest,
    OAuthRequest, OAuthRequestConfig,
};

#[cfg(test)]
mod test_support;

#[cfg(test)]
mod tests;
