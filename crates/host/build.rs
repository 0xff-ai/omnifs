//! Build the embedded archive-extractor wasm component before the host
//! crate compiles.
//!
//! `crates/host/src/runtime/wasm_extractor.rs` `include_bytes!`s
//! `target/wasm32-wasip2/release/omnifs_archive_extractor.wasm`. To
//! make `cargo build -p omnifs-host` work from a fresh checkout, this
//! script invokes cargo recursively to build the extractor first. The
//! recursive build uses a separate `CARGO_TARGET_DIR` to avoid
//! contention with the outer host build's locks, then copies the
//! resulting `.wasm` into the workspace's `target/wasm32-wasip2/release/`
//! so the `include_bytes!` path is satisfied.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("workspace root");
    let extractor_manifest = workspace_root
        .join("crates")
        .join("omnifs-archive-extractor")
        .join("Cargo.toml");
    let extractor_src = workspace_root
        .join("crates")
        .join("omnifs-archive-extractor")
        .join("src");
    let extractor_wit = workspace_root.join("wit").join("extractor");

    println!("cargo:rerun-if-changed={}", extractor_manifest.display());
    println!("cargo:rerun-if-changed={}", extractor_src.display());
    println!("cargo:rerun-if-changed={}", extractor_wit.display());
    println!("cargo:rerun-if-env-changed=OMNIFS_SKIP_EXTRACTOR_BUILD");

    if env::var_os("OMNIFS_SKIP_EXTRACTOR_BUILD").is_some() {
        // Escape hatch for environments that pre-stage the .wasm
        // (e.g. CI caching layers, distro packagers). The
        // `include_bytes!` consumer will fail loudly if the artifact
        // is missing.
        return;
    }

    let target_dir = workspace_root.join("target");
    let extractor_target = target_dir.join("extractor-build");
    let canonical_path = target_dir
        .join("wasm32-wasip2")
        .join("release")
        .join("omnifs_archive_extractor.wasm");

    // Match the outer build's profile so a debug host build doesn't
    // pay for a full release-mode wasm compile (and a release host
    // build still gets the smaller, faster wasm).
    let host_profile = env::var("PROFILE").unwrap_or_else(|_| "debug".into());
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut cmd = Command::new(&cargo);
    cmd.arg("build");
    if host_profile == "release" {
        cmd.arg("--release");
    }
    cmd.args(["--target", "wasm32-wasip2", "--manifest-path"])
        .arg(&extractor_manifest)
        .env("CARGO_TARGET_DIR", &extractor_target)
        // Clearing RUSTFLAGS keeps a host-side
        // `RUSTFLAGS=…native-target…` from leaking into the wasm build.
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_BUILD_RUSTFLAGS");
    let status = cmd.status();

    let status = match status {
        Ok(s) => s,
        Err(e) => panic!("failed to spawn cargo for extractor: {e}"),
    };
    assert!(
        status.success(),
        "extractor build failed with status: {status}"
    );

    let built = extractor_target
        .join("wasm32-wasip2")
        .join(if host_profile == "release" {
            "release"
        } else {
            "debug"
        })
        .join("omnifs_archive_extractor.wasm");
    assert!(
        built.exists(),
        "extractor build did not produce {}",
        built.display()
    );
    if let Some(parent) = canonical_path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        panic!("create {}: {e}", parent.display());
    }
    if let Err(e) = std::fs::copy(&built, &canonical_path) {
        panic!(
            "copy extractor {} -> {}: {e}",
            built.display(),
            canonical_path.display()
        );
    }
}
