//! Shared mount config file loading for CLI catalog and host registry.

use std::path::Path;

use omnifs_model::MountName;

use super::{ConfigError, InstanceConfig};

#[derive(Debug, thiserror::Error)]
pub enum MountConfigError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("invalid mount name `{mount}` in {}: {source}", path.display())]
    MountName {
        path: std::path::PathBuf,
        mount: String,
        source: omnifs_model::MountNameError,
    },
}

/// Parse a mount JSON file and validate its mount name.
pub fn load_mount_config(path: &Path) -> Result<InstanceConfig, MountConfigError> {
    let config = InstanceConfig::from_file(path)?;
    if let Err(source) = MountName::new(config.mount.clone()) {
        return Err(MountConfigError::MountName {
            path: path.to_path_buf(),
            mount: config.mount.clone(),
            source,
        });
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn rejects_invalid_mount_name_in_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("bad.json");
        fs::write(
            &path,
            r#"{"provider":"p.wasm","mount":"Bad-Name","config":{}}"#,
        )
        .expect("write config");

        let error = load_mount_config(&path).expect_err("invalid mount name");
        assert!(matches!(error, MountConfigError::MountName { .. }));
    }
}
