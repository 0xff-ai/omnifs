//! Build-time route introspection.
//!
//! Instantiates a built provider component through the real host runtime
//! harness ([`omnifs_host::Instance`]), calls `initialize` with an empty
//! config, reads the route table off `provider-info`, rebuilds the nested
//! tree from the flattened WIT encoding, and writes it as
//! `omnifs.routes.json` next to the provider source.
//!
//! This is the literal "introspect at build time" artifact: docs and CLI can
//! read a provider's path surface without a live daemon. It reuses the host's
//! component loader rather than building a second wasm host.
//!
//! Usage: `introspect_routes <provider.wasm> <out.json>`

use std::path::Path;
use std::process::ExitCode;

use omnifs_host::{Instance, component_engine};
use omnifs_wit::provider::types as wit;
use serde::Serialize;

/// Mirror of the SDK `RouteDescriptor`/`RouteManifest` JSON shape, rebuilt
/// from the flattened WIT list so the generated file is the nested tree a
/// reader expects. Field names and skip rules match
/// `omnifs_sdk::router::RouteDescriptor`.
#[derive(Serialize)]
struct RouteDescriptor {
    template: String,
    kind: RouteKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    representations: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    children: Vec<RouteDescriptor>,
}

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum RouteKind {
    Dir,
    File,
    TreeRef,
    Object,
    FileObject,
}

impl From<wit::RouteKind> for RouteKind {
    fn from(kind: wit::RouteKind) -> Self {
        match kind {
            wit::RouteKind::Dir => Self::Dir,
            wit::RouteKind::File => Self::File,
            wit::RouteKind::TreeRef => Self::TreeRef,
            wit::RouteKind::Object => Self::Object,
            wit::RouteKind::FileObject => Self::FileObject,
        }
    }
}

#[derive(Serialize)]
struct RouteManifest {
    routes: Vec<RouteDescriptor>,
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let (Some(wasm_path), Some(out_path)) = (args.next(), args.next()) else {
        eprintln!("usage: introspect_routes <provider.wasm> <out.json>");
        return ExitCode::FAILURE;
    };

    match run(Path::new(&wasm_path), Path::new(&out_path)) {
        Ok(Outcome::Wrote(count)) => {
            eprintln!(
                "wrote {} ({count} top-level routes)",
                Path::new(&out_path).display()
            );
            ExitCode::SUCCESS
        },
        // A provider whose config schema requires fields cannot `initialize`
        // under an empty config, so its route table is not introspectable at
        // build time. This is a known limitation, not a build failure: warn
        // and leave no file rather than aborting the whole provider build.
        Ok(Outcome::SkippedConfigGated(message)) => {
            eprintln!("introspect_routes {wasm_path}: skipped (config-gated): {message}");
            ExitCode::SUCCESS
        },
        Err(error) => {
            eprintln!("introspect_routes {wasm_path}: {error}");
            ExitCode::FAILURE
        },
    }
}

enum Outcome {
    Wrote(usize),
    SkippedConfigGated(String),
}

fn run(wasm_path: &Path, out_path: &Path) -> anyhow::Result<Outcome> {
    let engine = component_engine(|_| {}).map_err(|e| anyhow::anyhow!("engine: {e}"))?;
    // Empty config: providers must `initialize` under a default config for
    // introspection. A provider whose `start` hard-requires config fields will
    // surface that as an initialize error here, which is the honest signal.
    let instance = Instance::new(&engine, wasm_path, b"{}".to_vec(), &[])?;
    let ret = instance.initialize()?;
    let info = match ret.result {
        wit::OpResult::Initialize(result) => result.info,
        wit::OpResult::Error(error) if matches!(error.kind, wit::ErrorKind::InvalidInput) => {
            return Ok(Outcome::SkippedConfigGated(error.message));
        },
        other => anyhow::bail!("initialize returned unexpected result: {other:?}"),
    };

    let manifest = RouteManifest {
        routes: rebuild_tree(&info.routes),
    };
    let json = serde_json::to_string_pretty(&manifest)?;
    std::fs::write(out_path, format!("{json}\n"))?;
    Ok(Outcome::Wrote(manifest.routes.len()))
}

/// Rebuild the nested route tree from the flattened WIT list. Each entry's
/// `parent` is the index of its parent in the same list; every parent
/// precedes its children, so a single forward pass reattaches children.
fn rebuild_tree(flat: &[wit::RouteDescriptor]) -> Vec<RouteDescriptor> {
    // Build nodes carrying their original index so children can find parents.
    let mut nodes: Vec<(Option<u32>, RouteDescriptor)> = flat
        .iter()
        .map(|r| {
            (
                r.parent,
                RouteDescriptor {
                    template: r.template.clone(),
                    kind: r.kind.into(),
                    description: r.description.clone(),
                    representations: r.representations.clone(),
                    children: Vec::new(),
                },
            )
        })
        .collect();

    // Walk from the back so a popped child is already fully built before its
    // parent (which precedes it) consumes it.
    let mut roots = Vec::new();
    while let Some((parent, node)) = nodes.pop() {
        match parent {
            Some(idx) => {
                let idx = idx as usize;
                if idx < nodes.len() {
                    nodes[idx].1.children.insert(0, node);
                } else {
                    // Out-of-range parent: keep the node rather than drop it.
                    roots.insert(0, node);
                }
            },
            None => roots.insert(0, node),
        }
    }
    roots
}
