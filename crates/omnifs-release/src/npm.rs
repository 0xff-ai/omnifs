use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::Value;

const PACKAGE_FILES: &[&str] = &[
    "npm/omnifs/package.json",
    "npm/platform/darwin-arm64/package.json",
    "npm/platform/darwin-x64/package.json",
    "npm/platform/linux-arm64/package.json",
    "npm/platform/linux-x64/package.json",
];

const ROOT_PACKAGE: &str = "@0xff-ai/omnifs";

pub fn sync_versions(root: &Path, version: &str) -> Result<()> {
    for rel in PACKAGE_FILES {
        let path = root.join(rel);
        let mut value: Value = serde_json::from_str(
            &std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?,
        )?;
        value["version"] = Value::String(version.to_string());
        sync_root_optional_dependencies(&mut value, version);
        std::fs::write(
            &path,
            format!("{}\n", serde_json::to_string_pretty(&value)?),
        )?;
    }
    Ok(())
}

pub fn validate_synced(root: &Path, version: &str) -> Result<()> {
    for rel in PACKAGE_FILES {
        let path = root.join(rel);
        let value: Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
        let file_version = value
            .get("version")
            .and_then(Value::as_str)
            .with_context(|| format!("missing version in {}", path.display()))?;
        if file_version != version {
            bail!(
                "{} version {file_version} != workspace version {version}",
                path.display()
            );
        }
        validate_root_optional_dependencies(&value, version, &path)?;
    }
    Ok(())
}

pub fn validate_platforms(root: &Path) -> Result<()> {
    let script = root.join("npm/scripts/validate-platforms.mjs");
    if !script.is_file() {
        bail!("missing npm platform validator at {}", script.display());
    }

    let output = std::process::Command::new("node")
        .arg(&script)
        .current_dir(root)
        .output()
        .with_context(|| format!("run {}", script.display()))?;

    if output.status.success() {
        return Ok(());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!("npm platform validation failed:\n{stdout}{stderr}");
}

fn sync_root_optional_dependencies(value: &mut Value, version: &str) {
    if value
        .get("name")
        .and_then(Value::as_str)
        .is_some_and(|name| name == ROOT_PACKAGE)
        && let Some(deps) = value
            .get_mut("optionalDependencies")
            .and_then(Value::as_object_mut)
    {
        for dep_version in deps.values_mut() {
            *dep_version = Value::String(version.to_string());
        }
    }
}

fn validate_root_optional_dependencies(value: &Value, version: &str, path: &Path) -> Result<()> {
    if value
        .get("name")
        .and_then(Value::as_str)
        .is_none_or(|name| name != ROOT_PACKAGE)
    {
        return Ok(());
    }

    let Some(deps) = value.get("optionalDependencies").and_then(Value::as_object) else {
        bail!(
            "{} missing optionalDependencies for {ROOT_PACKAGE}",
            path.display()
        );
    };

    for (dep, dep_version) in deps {
        let Some(dep_version) = dep_version.as_str() else {
            bail!(
                "{} optionalDependencies.{dep} must be a version string",
                path.display()
            );
        };
        if dep_version != version {
            bail!(
                "{} optionalDependencies.{dep} version {dep_version} != workspace version {version}",
                path.display()
            );
        }
    }

    Ok(())
}
