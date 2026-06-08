//! omnifs-cli: Command-line interface for omnifs.
//!
//! Provides commands to mount and unmount the virtual filesystem,
//! as well as provider introspection utilities.

mod app_context;
mod auth;
mod capability;
mod catalog;
mod cli;
mod commands;
pub mod config;
mod container_name;
mod credential_target;
mod dev_mounts;
mod dev_support;
mod error;
mod image_ref;
mod inspector;
mod mount_report;
mod mount_tree;
mod native_runtime;
pub mod paths;
mod presentation;
mod proc_mounts;
mod provider_artifacts;
mod runtime;
mod runtime_mode;
mod runtime_state;
mod runtime_target;
mod session;
mod status;
mod style;
mod test_support;
mod token_source;

use clap::Parser;
use cli::Cli;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    init_tracing(cli.verbose);
    match run(cli).await {
        Ok(()) => {},
        Err(error) => {
            anstream::eprint!("{}", error::render(&error));
            std::process::exit(1);
        },
    }
}

fn init_tracing(verbose: u8) {
    use tracing_subscriber::{EnvFilter, fmt::format::FmtSpan};

    let verbosity = match verbose {
        0 => "warn",
        1 => "info",
        _ => "debug",
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(verbosity));
    let mut builder = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_env_filter(filter);
    if verbose >= 2 {
        builder = builder.with_span_events(FmtSpan::NEW | FmtSpan::CLOSE);
    }
    builder.init();
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    cli.command.run().await
}
