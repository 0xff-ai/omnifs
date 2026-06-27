//! omnifs repository tasks. Invoke via the `cargo xtask` alias, e.g.
//! `cargo xtask npm validate`.

mod npm;

use std::path::Path;

use anyhow::Result;
use clap::{Parser, Subcommand};

// ponytail: root and version come from cargo's build-time env, not a runtime
// Cargo.toml walk. xtask always runs via `cargo run` from this checkout, and
// `version.workspace = true` makes CARGO_PKG_VERSION the workspace version.
const WORKSPACE_VERSION: &str = env!("CARGO_PKG_VERSION");

fn repo_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/xtask sits two levels below the repo root")
}

#[derive(Parser)]
#[command(about = "omnifs repository tasks")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// npm platform package metadata tasks.
    Npm {
        #[command(subcommand)]
        command: NpmCommand,
    },
}

#[derive(Subcommand)]
enum NpmCommand {
    /// Sync npm package versions from the Cargo workspace version.
    Sync {
        /// Version to sync to (defaults to the Cargo workspace version).
        version: Option<String>,
    },
    /// Validate npm platform metadata and package manifests.
    Validate,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let root = repo_root();
    match Cli::parse().command {
        Command::Npm { command } => match command {
            NpmCommand::Sync { version } => {
                npm::sync(root, version.as_deref().unwrap_or(WORKSPACE_VERSION))
            },
            NpmCommand::Validate => npm::validate(root, WORKSPACE_VERSION),
        },
    }
}
