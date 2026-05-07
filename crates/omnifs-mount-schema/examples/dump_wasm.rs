//! Example: read the `omnifs.provider-manifest.v1` custom section from a
//! wasm file and print a summary of handler and mutation records.
//!
//! Run with:
//!   cargo run -p omnifs-mount-schema --example dump_wasm -- \
//!     target/wasm32-wasip2/debug/omnifs_provider_github.wasm

use std::env;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use omnifs_mount_schema as mts;

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    let path = args
        .get(1)
        .ok_or_else(|| anyhow!("usage: dump_wasm <path-to-wasm>"))?;
    let bytes = fs::read(Path::new(path)).with_context(|| format!("reading {path}"))?;

    let section_bytes =
        mts::read_manifest_section(&bytes).context("reading provider-manifest section")?;
    if section_bytes.is_empty() {
        bail!("no {} custom section found", mts::MANIFEST_SECTION_NAME);
    }

    let mut raw_records = Vec::new();
    let mut unknown = 0usize;
    let mut subtree_routes = 0usize;
    for record in mts::ManifestRecordIter::new(&section_bytes) {
        let record = record?;
        if matches!(record, mts::ManifestRecord::SubtreeRoute(_)) {
            subtree_routes += 1;
        }
        if let mts::ManifestRecord::Unknown { tag, .. } = &record {
            unknown += 1;
            eprintln!("unknown tag 0x{tag:02x}");
        }
        raw_records.push(record);
    }

    let resolved = mts::resolve_manifest(raw_records)
        .map_err(|error| anyhow!("resolving manifest: {error}"))?;
    for handler in &resolved.handlers {
        println!(
            "handler: {} [{}] -> {}",
            handler.path_template,
            handler_kind_label(&handler.handler_kind),
            handler.handler_name,
        );
    }
    for mutation in &resolved.mutations {
        println!("mutation: {}", mutation.path_template);
    }

    println!(
        "summary: handlers={} mutations={} subtree_routes={subtree_routes} unknown={unknown}",
        resolved.handlers.len(),
        resolved.mutations.len(),
    );
    Ok(())
}

fn handler_kind_label(kind: &mts::HandlerKindRecord) -> &'static str {
    match kind {
        mts::HandlerKindRecord::Dir => "dir",
        mts::HandlerKindRecord::File => "file",
        mts::HandlerKindRecord::TreeRef => "treeref",
        mts::HandlerKindRecord::Subtree => "subtree",
    }
}
