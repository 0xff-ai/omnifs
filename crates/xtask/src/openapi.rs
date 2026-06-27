use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

const SPEC_PATH: &str = "crates/omnifs-api/openapi/daemon.json";

/// Regenerate the checked-in OpenAPI spec from the daemon implementation.
pub fn generate(root: &Path) -> Result<()> {
    let spec = spec_from_implementation(root)?;
    let path = root.join(SPEC_PATH);
    fs::write(&path, spec).with_context(|| format!("write {}", path.display()))?;
    println!("OpenAPI spec written");
    Ok(())
}

/// Fail if the checked-in OpenAPI spec differs from the daemon implementation.
pub fn check(root: &Path) -> Result<()> {
    let implemented = spec_from_implementation(root)?;
    let checked_in =
        fs::read_to_string(root.join(SPEC_PATH)).with_context(|| format!("read {SPEC_PATH}"))?;
    if checked_in != implemented {
        bail!("checked-in OpenAPI spec is stale; run `just openapi`");
    }
    println!("OpenAPI spec is current");
    Ok(())
}

fn spec_from_implementation(root: &Path) -> Result<String> {
    let output = Command::new("cargo")
        .args([
            "run",
            "-p",
            "omnifs-daemon",
            "--bin",
            "omnifsd-openapi",
            "--quiet",
        ])
        .current_dir(root)
        .output()
        .context("run omnifsd-openapi")?;
    if !output.status.success() {
        bail!(
            "omnifsd-openapi failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8(output.stdout).context("omnifsd-openapi produced non-utf8 output")
}
