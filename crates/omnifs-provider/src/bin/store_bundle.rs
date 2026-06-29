//! Build a content-addressed provider-store bundle from built provider WASM.

use std::path::PathBuf;

use omnifs_provider::{Artifact, ProviderStore};

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse()?;
    if args.out.exists() {
        std::fs::remove_dir_all(&args.out)?;
    }
    std::fs::create_dir_all(&args.out)?;

    let store = ProviderStore::new(&args.out);
    for wasm in &args.wasms {
        let artifact = Artifact::from_file(wasm)?;
        store.add_artifact(artifact)?;
    }

    let index = store.read_index()?;
    eprintln!(
        "wrote {} provider artifact(s) to {}",
        index.providers.len(),
        store.root().display()
    );
    Ok(())
}

struct Args {
    out: PathBuf,
    wasms: Vec<PathBuf>,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut out = None;
        let mut wasms = Vec::new();
        let mut raw = std::env::args_os().skip(1);
        while let Some(arg) = raw.next() {
            if arg == "--out" {
                let value = raw
                    .next()
                    .ok_or_else(|| "--out requires a directory".to_string())?;
                out = Some(PathBuf::from(value));
            } else {
                wasms.push(PathBuf::from(arg));
            }
        }
        let out = out.ok_or_else(|| {
            "usage: omnifs-provider-store-bundle --out <dir> <provider.wasm>...".to_string()
        })?;
        if wasms.is_empty() {
            return Err("at least one provider WASM is required".to_string());
        }
        Ok(Self { out, wasms })
    }
}
