//! Auth wire types for reading embedded provider auth manifests.

pub use omnifs_mount_schema::{
    AuthManifest, AuthScheme, DeviceCodeConfig, KeyValue, OAuthFlow, OauthScheme,
    PkceLoopbackConfig, PkceManualCodeConfig, StaticTokenScheme, TokenEndpointAuthMethod,
};
