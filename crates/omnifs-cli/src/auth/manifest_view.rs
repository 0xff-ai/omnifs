use omnifs_auth::AuthManifest;

const DEFAULT_STATIC_SCHEME: &str = "static-token";

pub(crate) fn static_token_scheme_key(
    manifest: &AuthManifest,
    requested: Option<&str>,
) -> anyhow::Result<String> {
    if let Some(requested) = requested {
        return Ok(requested.to_owned());
    }
    let Some(first) = manifest.first_static_scheme_key() else {
        return Ok(DEFAULT_STATIC_SCHEME.to_owned());
    };
    if manifest.static_scheme_count() > 1 {
        anyhow::bail!("multiple static-token schemes are declared; pass --scheme");
    }
    Ok(first.to_owned())
}
