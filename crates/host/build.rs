//! Validate the embedded archive tool wasm artifact before the host
//! crate compiles.
//!
//! The host embeds `target/wasm32-wasip2/release/omnifs_tool_archive.wasm`.
//! Building that component is an explicit workspace/Docker step; this script
//! only stages a caller-supplied artifact and reports a clear error when the
//! artifact is missing.

use std::env;
use std::path::{Path, PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("workspace root");
    let canonical_path = workspace_root
        .join("target")
        .join("wasm32-wasip2")
        .join("release")
        .join("omnifs_tool_archive.wasm");

    println!("cargo:rerun-if-changed={}", canonical_path.display());
    println!("cargo:rerun-if-env-changed=OMNIFS_EXTRACTOR_WASM");
    println!("cargo:rerun-if-env-changed=OMNIFS_SKIP_EXTRACTOR_CHECK");

    if env::var_os("OMNIFS_SKIP_EXTRACTOR_CHECK").is_some() {
        return;
    }

    if let Some(source) = env::var_os("OMNIFS_EXTRACTOR_WASM") {
        let source = PathBuf::from(source);
        if source != canonical_path {
            println!("cargo:rerun-if-changed={}", source.display());
            assert!(
                source.exists(),
                "OMNIFS_EXTRACTOR_WASM points to missing artifact: {}",
                source.display()
            );
            if let Some(parent) = canonical_path.parent() {
                std::fs::create_dir_all(parent).unwrap_or_else(|e| {
                    panic!(
                        "create extractor artifact directory {}: {e}",
                        parent.display()
                    )
                });
            }
            std::fs::copy(&source, &canonical_path).unwrap_or_else(|e| {
                panic!(
                    "stage extractor artifact {} -> {}: {e}",
                    source.display(),
                    canonical_path.display()
                )
            });
        }
    }

    assert!(
        canonical_path.exists(),
        "missing archive extractor wasm at {}. Run `just build-providers` or \
         `cargo build -p omnifs-tool-archive --target wasm32-wasip2 --release` before \
         building `omnifs-host`.",
        canonical_path.display()
    );
}
